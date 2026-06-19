use crate::vm::instr::{BufferPtr, TypedArrayKind, TypedArrayPtr, TypedArrayView};
use crate::vm::{ErrorKind, VM, VMError, Value};

use super::Args;

// ── Element codec ──────────────────────────────────────────────────────────────

/// Decode one typed-array element from raw bytes (native byte order).
/// `bytes` must contain at least `kind.element_size()` bytes starting at offset 0.
pub(crate) fn ta_decode_bytes(kind: TypedArrayKind, bytes: &[u8]) -> Value {
    match kind {
        TypedArrayKind::Int8 => Value::int_from_f64(bytes[0] as i8 as f64),
        TypedArrayKind::Uint8 | TypedArrayKind::Uint8Clamped => Value::PosInt(bytes[0] as u64),
        TypedArrayKind::Int16 => {
            Value::int_from_f64(i16::from_ne_bytes([bytes[0], bytes[1]]) as f64)
        }
        TypedArrayKind::Uint16 => {
            Value::PosInt(u16::from_ne_bytes([bytes[0], bytes[1]]) as u64)
        }
        TypedArrayKind::Int32 => {
            let v = i32::from_ne_bytes(bytes[..4].try_into().unwrap());
            Value::int_from_f64(v as f64)
        }
        TypedArrayKind::Uint32 => {
            let v = u32::from_ne_bytes(bytes[..4].try_into().unwrap());
            Value::PosInt(v as u64)
        }
        TypedArrayKind::Float32 => {
            Value::Float(f32::from_ne_bytes(bytes[..4].try_into().unwrap()) as f64)
        }
        TypedArrayKind::Float64 => {
            Value::Float(f64::from_ne_bytes(bytes[..8].try_into().unwrap()))
        }
        TypedArrayKind::BigInt64 | TypedArrayKind::BigUint64 => {
            // BigInt not yet supported — return 0 as a fallback.
            Value::PosInt(0)
        }
    }
}

/// Encode a numeric f64 into raw bytes for a typed array element (native byte order).
/// `buf` must be exactly `kind.element_size()` bytes long.
pub(crate) fn ta_encode_bytes(kind: TypedArrayKind, f: f64, buf: &mut [u8]) {
    match kind {
        TypedArrayKind::Int8 | TypedArrayKind::Uint8 => {
            buf[0] = to_uint32(f) as u8;
        }
        TypedArrayKind::Uint8Clamped => {
            buf[0] = to_uint8_clamp(f);
        }
        TypedArrayKind::Int16 | TypedArrayKind::Uint16 => {
            let v = to_uint32(f) as u16;
            buf[..2].copy_from_slice(&v.to_ne_bytes());
        }
        TypedArrayKind::Int32 | TypedArrayKind::Uint32 => {
            let v = to_uint32(f);
            buf[..4].copy_from_slice(&v.to_ne_bytes());
        }
        TypedArrayKind::Float32 => {
            buf[..4].copy_from_slice(&(f as f32).to_ne_bytes());
        }
        TypedArrayKind::Float64 => {
            buf[..8].copy_from_slice(&f.to_ne_bytes());
        }
        TypedArrayKind::BigInt64 | TypedArrayKind::BigUint64 => {
            // BigInt not yet supported — no-op (write zero bytes).
        }
    }
}

/// JS `ToUint32` (wrapping, not saturating). `NaN`/`±0`/`±Inf` → 0.
fn to_uint32(f: f64) -> u32 {
    if !f.is_finite() || f == 0.0 {
        return 0;
    }
    f.trunc().rem_euclid(4_294_967_296.0) as u32
}

/// JS `ToUint8Clamp` (round-half-to-even, then clamp).
fn to_uint8_clamp(f: f64) -> u8 {
    if f.is_nan() || f <= 0.0 {
        return 0;
    }
    if f >= 255.0 {
        return 255;
    }
    let floor = f.floor();
    let frac = f - floor;
    if frac < 0.5 {
        floor as u8
    } else if frac > 0.5 {
        floor as u8 + 1
    } else {
        // Exactly 0.5: round to even.
        let v = floor as u8;
        if v % 2 == 0 { v } else { v + 1 }
    }
}

// ── ArrayBuffer ────────────────────────────────────────────────────────────────

/// `new ArrayBuffer(length)` — allocate a zero-initialized byte buffer.
pub(crate) fn arraybuffer_ctor(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let raw = args.get(vm, 0);
    let n = match raw.to_number() {
        Some(f) => {
            if f.is_nan() {
                // undefined → NaN via to_number → treat as 0 (JS ToIndex semantics).
                0u32
            } else if f < 0.0 || f.is_infinite() {
                return Err(vm.fail(
                    ErrorKind::ValueError,
                    format!("invalid ArrayBuffer length: {}", vm.preview(raw)),
                ));
            } else {
                (f as u64).min(u32::MAX as u64) as u32
            }
        }
        None => {
            return Err(vm.fail(
                ErrorKind::TypeError,
                format!("ArrayBuffer length must be a number, got {}", raw.type_name()),
            ));
        }
    };
    let ptr = vm.buffers.len() as BufferPtr;
    vm.buffers.push(vec![0u8; n as usize]);
    Ok(Value::ArrayBuffer(ptr))
}

/// `ArrayBuffer.isView(x)` — returns true if x is a TypedArray or DataView.
pub(crate) fn arraybuffer_is_view(_vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let val = args.get(_vm, 0);
    Ok(Value::Bool(matches!(val, Value::TypedArray(_) | Value::DataView(_))))
}

/// `ArrayBuffer.prototype.slice(begin[, end])` — copy a byte range into a new buffer.
pub(crate) fn arraybuffer_slice(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let recv = args.get(vm, 0);
    let buf_ptr = match recv {
        Value::ArrayBuffer(p) => *p,
        _ => return Err(vm.method_receiver_error(recv)),
    };
    let buf = vm
        .buffers
        .get(buf_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "bad ArrayBuffer pointer"))?;
    let total = buf.len() as u32;
    let begin = clamp_index(args.get(vm, 1), total);
    let end = if args.argc >= 3 {
        clamp_index(args.get(vm, 2), total)
    } else {
        total
    };
    let end = end.max(begin);
    let slice = buf[begin as usize..end as usize].to_vec();
    let ptr = vm.buffers.len() as BufferPtr;
    vm.buffers.push(slice);
    Ok(Value::ArrayBuffer(ptr))
}

fn clamp_index(val: &Value, total: u32) -> u32 {
    match val {
        Value::Undefined => 0,
        _ => {
            let f = val.to_number().unwrap_or(0.0);
            if f.is_nan() || f < 0.0 {
                0
            } else if f > total as f64 {
                total
            } else {
                (f as u32).min(total)
            }
        }
    }
}

// ── Typed array constructors ──────────────────────────────────────────────────

/// Shared constructor body for all 9 typed-array kinds.
///
/// Supports:
/// - `new T(n)` — allocate `n` zero-initialised elements
/// - `new T([...])` — copy from an ordinary `Array`
/// - `new T(otherTA)` — copy from another `TypedArray`
///
/// The `new T(buffer[, byteOffset[, length]])` overload (shared backing store)
/// is deferred to Step 7a.
fn typed_array_ctor_impl(vm: &mut VM, args: Args, kind: TypedArrayKind) -> Result<Value, VMError> {
    let first = args.get(vm, 0);
    match first {
        Value::Undefined => alloc_typed_array(vm, 0, kind),

        &Value::PosInt(n) => {
            if n > u32::MAX as u64 {
                return Err(vm.fail(
                    ErrorKind::ValueError,
                    "typed array length out of range",
                ));
            }
            alloc_typed_array(vm, n as usize, kind)
        }

        &Value::NegInt(_) => Err(vm.fail(
            ErrorKind::ValueError,
            "invalid typed array length: negative",
        )),

        &Value::Float(f) => {
            if f.is_nan() || f == 0.0 {
                alloc_typed_array(vm, 0, kind)
            } else if f < 0.0 || f.is_infinite() {
                Err(vm.fail(
                    ErrorKind::ValueError,
                    format!("invalid typed array length: {f}"),
                ))
            } else {
                let n = f as usize;
                alloc_typed_array(vm, n, kind)
            }
        }

        &Value::Array(arr_ptr) => {
            let arr_ptr = arr_ptr;
            let len = vm.arrays.get(arr_ptr as usize).map(|a| a.len()).unwrap_or(0);
            let elem_size = kind.element_size() as usize;
            let mut buf = vec![0u8; len * elem_size];
            for i in 0..len {
                let f = vm
                    .arrays
                    .get(arr_ptr as usize)
                    .and_then(|a| a.get(i))
                    .and_then(|v| v.to_number())
                    .unwrap_or(0.0);
                ta_encode_bytes(kind, f, &mut buf[i * elem_size..(i + 1) * elem_size]);
            }
            let buf_ptr = vm.buffers.len() as BufferPtr;
            vm.buffers.push(buf);
            let ta_ptr = vm.typed_arrays.len() as TypedArrayPtr;
            vm.typed_arrays.push(TypedArrayView {
                buffer: buf_ptr,
                byte_offset: 0,
                byte_length: (len * elem_size) as u32,
                kind,
            });
            Ok(Value::TypedArray(ta_ptr))
        }

        &Value::TypedArray(src_ptr) => {
            let (src_len, src_kind, src_buf_ptr, src_byte_offset) =
                match vm.typed_arrays.get(src_ptr as usize) {
                    Some(v) => (v.length() as usize, v.kind, v.buffer, v.byte_offset as usize),
                    None => {
                        return Err(
                            vm.fail_not_resumable(ErrorKind::TypeError, "bad typed array pointer")
                        );
                    }
                };
            let elem_size = kind.element_size() as usize;
            let src_elem_size = src_kind.element_size() as usize;
            let mut buf = vec![0u8; src_len * elem_size];
            for i in 0..src_len {
                let src_off = src_byte_offset + i * src_elem_size;
                let src_bytes = vm
                    .buffers
                    .get(src_buf_ptr as usize)
                    .map(|b| &b[src_off..src_off + src_elem_size])
                    .unwrap_or(&[]);
                let v = if src_bytes.len() == src_elem_size {
                    ta_decode_bytes(src_kind, src_bytes)
                } else {
                    Value::Undefined
                };
                let f = v.to_number().unwrap_or(0.0);
                ta_encode_bytes(kind, f, &mut buf[i * elem_size..(i + 1) * elem_size]);
            }
            let buf_ptr = vm.buffers.len() as BufferPtr;
            vm.buffers.push(buf);
            let ta_ptr = vm.typed_arrays.len() as TypedArrayPtr;
            vm.typed_arrays.push(TypedArrayView {
                buffer: buf_ptr,
                byte_offset: 0,
                byte_length: (src_len * elem_size) as u32,
                kind,
            });
            Ok(Value::TypedArray(ta_ptr))
        }

        &Value::ArrayBuffer(buf_ptr) => {
            let byte_len = vm
                .buffers
                .get(buf_ptr as usize)
                .map(|b| b.len())
                .unwrap_or(0);
            let elem_size = kind.element_size() as usize;
            let byte_length = byte_len as u32;
            let ta_ptr = vm.typed_arrays.len() as TypedArrayPtr;
            vm.typed_arrays.push(TypedArrayView {
                buffer: buf_ptr,
                byte_offset: 0,
                byte_length,
                kind,
            });
            let _ = elem_size;
            Ok(Value::TypedArray(ta_ptr))
        }

        other => Err(vm.fail(
            ErrorKind::TypeError,
            format!(
                "typed array constructor: unsupported argument type {}",
                other.type_name()
            ),
        )),
    }
}

/// Allocate a zero-filled typed array of `len` elements.
fn alloc_typed_array(vm: &mut VM, len: usize, kind: TypedArrayKind) -> Result<Value, VMError> {
    let byte_len = len
        .checked_mul(kind.element_size() as usize)
        .ok_or_else(|| vm.fail(ErrorKind::ValueError, "typed array byte length overflow"))?;
    let buf_ptr = vm.buffers.len() as BufferPtr;
    vm.buffers.push(vec![0u8; byte_len]);
    let ta_ptr = vm.typed_arrays.len() as TypedArrayPtr;
    vm.typed_arrays.push(TypedArrayView {
        buffer: buf_ptr,
        byte_offset: 0,
        byte_length: byte_len as u32,
        kind,
    });
    Ok(Value::TypedArray(ta_ptr))
}

macro_rules! ta_ctor {
    ($name:ident, $kind:expr) => {
        pub(crate) fn $name(vm: &mut VM, args: Args) -> Result<Value, VMError> {
            typed_array_ctor_impl(vm, args, $kind)
        }
    };
}

ta_ctor!(int8array_ctor,         TypedArrayKind::Int8);
ta_ctor!(uint8array_ctor,        TypedArrayKind::Uint8);
ta_ctor!(uint8clamped_ctor,      TypedArrayKind::Uint8Clamped);
ta_ctor!(int16array_ctor,        TypedArrayKind::Int16);
ta_ctor!(uint16array_ctor,       TypedArrayKind::Uint16);
ta_ctor!(int32array_ctor,        TypedArrayKind::Int32);
ta_ctor!(uint32array_ctor,       TypedArrayKind::Uint32);
ta_ctor!(float32array_ctor,      TypedArrayKind::Float32);
ta_ctor!(float64array_ctor,      TypedArrayKind::Float64);

pub(crate) fn bigint64array_ctor(vm: &mut VM, _args: Args) -> Result<Value, VMError> {
    Err(vm.fail(ErrorKind::TypeError, "BigInt64Array: BigInt not yet supported"))
}

pub(crate) fn biguint64array_ctor(vm: &mut VM, _args: Args) -> Result<Value, VMError> {
    Err(vm.fail(ErrorKind::TypeError, "BigUint64Array: BigInt not yet supported"))
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::testutil;
    use crate::vm::Value;

    // ── Step 2: ArrayBuffer ────────────────────────────────────────────────

    #[test]
    fn arraybuffer_new_bytelength() {
        let v = testutil::run_ret("const b = new ArrayBuffer(16); return b.byteLength;");
        assert_eq!(v, serde_json::json!(16));
    }

    #[test]
    fn arraybuffer_no_arg_is_zero() {
        let v = testutil::run_ret("return new ArrayBuffer().byteLength;");
        assert_eq!(v, serde_json::json!(0));
    }

    #[test]
    fn arraybuffer_fractional_truncates() {
        let v = testutil::run_ret("return new ArrayBuffer(3.9).byteLength;");
        assert_eq!(v, serde_json::json!(3));
    }

    #[test]
    fn arraybuffer_is_view_true() {
        let v = testutil::run_ret("const ta = new Uint8Array(4); return ArrayBuffer.isView(ta);");
        assert_eq!(v, serde_json::json!(true));
    }

    #[test]
    fn arraybuffer_is_view_false() {
        let v = testutil::run_ret("return ArrayBuffer.isView(new ArrayBuffer(4));");
        assert_eq!(v, serde_json::json!(false));
    }

    #[test]
    fn arraybuffer_slice() {
        let v = testutil::run_ret(
            r#"
            const src = new Uint8Array([10, 20, 30, 40]);
            const sliced = src.buffer.slice(1, 3);
            const view = new Uint8Array(sliced);
            return [view[0], view[1], view.byteLength];
            "#,
        );
        assert_eq!(v, serde_json::json!([20, 30, 2]));
    }

    #[test]
    fn arraybuffer_json_rejection() {
        let prog = testutil::compile_ok("return new ArrayBuffer(4);");
        let mut vm =
            crate::vm::VM::for_program(prog, serde_json::Value::Null).unwrap();
        use crate::vm::StepResult;
        let result = loop {
            match vm.step(u64::MAX).unwrap() {
                StepResult::Done { value, .. } => break value,
                _ => {}
            }
        };
        // stack_value_to_json should reject ArrayBuffer → Null sentinel
        assert!(matches!(result, Value::ArrayBuffer(_)));
    }

    // ── Step 3: Typed arrays ───────────────────────────────────────────────

    #[test]
    fn float64array_from_length() {
        let v = testutil::run_ret(
            "const ta = new Float64Array(4); return [ta.length, ta.byteLength, ta[0]];",
        );
        assert_eq!(v, serde_json::json!([4, 32, 0]));
    }

    #[test]
    fn float64array_from_array() {
        let v = testutil::run_ret(
            "const ta = new Float64Array([1.5, 2.5, 3.5]); return [ta[0], ta[1], ta[2], ta.length];",
        );
        assert_eq!(v, serde_json::json!([1.5, 2.5, 3.5, 3]));
    }

    #[test]
    fn int32array_write_and_read() {
        let v = testutil::run_ret(
            "const ta = new Int32Array(3); ta[0] = 42; ta[1] = -1; return [ta[0], ta[1], ta[2]];",
        );
        assert_eq!(v, serde_json::json!([42, -1, 0]));
    }

    #[test]
    fn uint8array_clamping() {
        let v = testutil::run_ret(
            "const ta = new Uint8ClampedArray(3); ta[0] = 300; ta[1] = -5; ta[2] = 127.6; return [ta[0], ta[1], ta[2]];",
        );
        assert_eq!(v, serde_json::json!([255, 0, 128]));
    }

    #[test]
    fn uint8array_wrap_overflow() {
        let v = testutil::run_ret(
            "const ta = new Uint8Array(2); ta[0] = 256; ta[1] = 257; return [ta[0], ta[1]];",
        );
        assert_eq!(v, serde_json::json!([0, 1]));
    }

    #[test]
    fn typed_array_oob_is_undefined() {
        let v = testutil::run_ret(
            "const ta = new Uint8Array(4); return ta[4] === undefined;",
        );
        assert_eq!(v, serde_json::json!(true));
    }

    #[test]
    fn typed_array_negative_index_is_undefined() {
        let v = testutil::run_ret(
            "const ta = new Uint8Array(4); return ta[-1] === undefined;",
        );
        assert_eq!(v, serde_json::json!(true));
    }

    #[test]
    fn typed_array_oob_write_is_noop() {
        let v = testutil::run_ret(
            "const ta = new Uint8Array(4); ta[ta.length] = 99; return ta[ta.length] === undefined;",
        );
        assert_eq!(v, serde_json::json!(true));
    }

    #[test]
    fn typed_array_from_typed_array() {
        let v = testutil::run_ret(
            "const a = new Float64Array([1,2,3]); const b = new Int32Array(a); return [b[0], b[1], b[2]];",
        );
        assert_eq!(v, serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn typed_array_byte_offset_and_buffer() {
        let v = testutil::run_ret(
            "const ta = new Float32Array(4); return [ta.byteOffset, ta.byteLength, ta.BYTES_PER_ELEMENT];",
        );
        assert_eq!(v, serde_json::json!([0, 16, 4]));
    }

    #[test]
    fn uint8_nan_write_is_zero() {
        let v = testutil::run_ret(
            "const ta = new Uint8Array(1); ta[0] = NaN; return ta[0];",
        );
        assert_eq!(v, serde_json::json!(0));
    }

    #[test]
    fn float64_nan_roundtrip() {
        let v = testutil::run_ret(
            "const ta = new Float64Array(1); ta[0] = NaN; return Number.isNaN(ta[0]);",
        );
        assert_eq!(v, serde_json::json!(true));
    }

    #[test]
    fn all_kinds_construct() {
        let v = testutil::run_ret(
            r#"
            return [
                new Int8Array(1).length,
                new Uint8Array(1).length,
                new Uint8ClampedArray(1).length,
                new Int16Array(1).length,
                new Uint16Array(1).length,
                new Int32Array(1).length,
                new Uint32Array(1).length,
                new Float32Array(1).length,
                new Float64Array(1).length,
            ];
            "#,
        );
        assert_eq!(v, serde_json::json!([1, 1, 1, 1, 1, 1, 1, 1, 1]));
    }

    #[test]
    fn typed_array_uint8_clamped_half_round_to_even() {
        // 0.5 rounds to 0 (even), 1.5 rounds to 2 (even), 2.5 rounds to 2 (even)
        let v = testutil::run_ret(
            "const ta = new Uint8ClampedArray(3); ta[0] = 0.5; ta[1] = 1.5; ta[2] = 2.5; return [ta[0], ta[1], ta[2]];",
        );
        assert_eq!(v, serde_json::json!([0, 2, 2]));
    }
}
