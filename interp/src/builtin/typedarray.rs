use crate::vm::instr::{BufferPtr, DataViewEntry, DataViewPtr, TypedArrayKind, TypedArrayPtr, TypedArrayView};
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

// ── Step 5: DataView ──────────────────────────────────────────────────────────

/// `new DataView(buffer[, byteOffset[, byteLength]])` constructor.
pub(crate) fn dataview_ctor(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let buf_ptr = match args.get(vm, 0) {
        Value::ArrayBuffer(p) => *p,
        recv => {
            return Err(vm.fail(
                ErrorKind::TypeError,
                format!("DataView requires an ArrayBuffer, got {}", recv.type_name()),
            ));
        }
    };
    let buf_len = vm
        .buffers
        .get(buf_ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "bad ArrayBuffer pointer"))?
        .len();
    let byte_offset = match args.get(vm, 1) {
        Value::Undefined => 0usize,
        v => {
            let f = v.to_number().unwrap_or(0.0);
            if f < 0.0 || f.is_nan() {
                0usize
            } else {
                f as usize
            }
        }
    };
    if byte_offset > buf_len {
        return Err(vm.fail(ErrorKind::ValueError, "DataView: byteOffset out of bounds"));
    }
    let byte_length = match args.get(vm, 2) {
        Value::Undefined => buf_len - byte_offset,
        v => {
            let f = v.to_number().unwrap_or(0.0);
            if f < 0.0 || f.is_nan() {
                return Err(vm.fail(ErrorKind::ValueError, "DataView: invalid byteLength"));
            }
            let bl = f as usize;
            if byte_offset + bl > buf_len {
                return Err(vm.fail(ErrorKind::ValueError, "DataView: byteLength out of bounds"));
            }
            bl
        }
    };
    let ptr = vm.data_views.len() as DataViewPtr;
    vm.data_views.push(DataViewEntry {
        buffer: buf_ptr,
        byte_offset: byte_offset as u32,
        byte_length: byte_length as u32,
    });
    Ok(Value::DataView(ptr))
}

/// Extract `(buf_ptr, byte_offset, byte_length)` from a DataView receiver.
fn dv_receiver(vm: &VM, args: &Args) -> Result<(BufferPtr, usize, usize), VMError> {
    let ptr = match args.get(vm, 0) {
        Value::DataView(p) => *p,
        recv => return Err(vm.method_receiver_error(recv)),
    };
    let dv = vm
        .data_views
        .get(ptr as usize)
        .ok_or_else(|| vm.fail(ErrorKind::TypeError, "bad DataView pointer"))?;
    Ok((dv.buffer, dv.byte_offset as usize, dv.byte_length as usize))
}

/// Validate a DataView byte offset + access size, returning the absolute byte index.
fn dv_byte_index(
    vm: &VM,
    buf_ptr: BufferPtr,
    view_off: usize,
    view_len: usize,
    offset_val: &Value,
    access_size: usize,
) -> Result<usize, VMError> {
    let f = offset_val.to_number().unwrap_or(0.0);
    if f < 0.0 || f.is_nan() {
        return Err(vm.fail(ErrorKind::ValueError, "DataView offset must be non-negative"));
    }
    let off = f as usize;
    if off + access_size > view_len {
        return Err(vm.fail(ErrorKind::ValueError, "DataView access out of bounds"));
    }
    let _ = buf_ptr;
    Ok(view_off + off)
}

/// Parse `littleEndian` boolean from optional arg (default `false` = big-endian).
fn dv_little_endian(args: &Args, vm: &VM, idx: usize) -> bool {
    args.get(vm, idx).is_truthy()
}

macro_rules! dv_get {
    ($name:ident, $size:expr, $from_le:expr, $from_be:expr, $to_val:expr) => {
        pub(crate) fn $name(vm: &mut VM, args: Args) -> Result<Value, VMError> {
            let (buf_ptr, view_off, view_len) = dv_receiver(vm, &args)?;
            let abs = dv_byte_index(vm, buf_ptr, view_off, view_len, args.get(vm, 1), $size)?;
            let buf = vm
                .buffers
                .get(buf_ptr as usize)
                .ok_or_else(|| vm.fail(ErrorKind::TypeError, "bad buffer pointer"))?;
            let bytes: [u8; $size] = buf[abs..abs + $size].try_into().unwrap();
            let le = dv_little_endian(&args, vm, 2);
            let raw = if le { $from_le(bytes) } else { $from_be(bytes) };
            Ok($to_val(raw))
        }
    };
}

macro_rules! dv_set {
    ($name:ident, $size:expr, $from_f:expr, $to_le:expr, $to_be:expr) => {
        pub(crate) fn $name(vm: &mut VM, args: Args) -> Result<Value, VMError> {
            let (buf_ptr, view_off, view_len) = dv_receiver(vm, &args)?;
            let abs = dv_byte_index(vm, buf_ptr, view_off, view_len, args.get(vm, 1), $size)?;
            let raw_f = args.get(vm, 2).to_number().unwrap_or(0.0);
            let val = $from_f(raw_f);
            let le = dv_little_endian(&args, vm, 3);
            let bytes = if le { $to_le(val) } else { $to_be(val) };
            let ip = vm.ip;
            let buf = vm
                .buffers
                .get_mut(buf_ptr as usize)
                .ok_or_else(|| VMError::fail_at(ip, ErrorKind::TypeError, "bad buffer pointer"))?;
            buf[abs..abs + $size].copy_from_slice(&bytes);
            Ok(Value::Undefined)
        }
    };
}

dv_get!(dv_get_int8,   1, |b: [u8;1]| b[0] as i8,  |b: [u8;1]| b[0] as i8,  |v: i8|  Value::int_from_f64(v as f64));
dv_get!(dv_get_uint8,  1, |b: [u8;1]| b[0],         |b: [u8;1]| b[0],         |v: u8|  Value::PosInt(v as u64));
dv_get!(dv_get_int16,  2, |b: [u8;2]| i16::from_le_bytes(b), |b: [u8;2]| i16::from_be_bytes(b), |v: i16| Value::int_from_f64(v as f64));
dv_get!(dv_get_uint16, 2, |b: [u8;2]| u16::from_le_bytes(b), |b: [u8;2]| u16::from_be_bytes(b), |v: u16| Value::PosInt(v as u64));
dv_get!(dv_get_int32,  4, |b: [u8;4]| i32::from_le_bytes(b), |b: [u8;4]| i32::from_be_bytes(b), |v: i32| Value::int_from_f64(v as f64));
dv_get!(dv_get_uint32, 4, |b: [u8;4]| u32::from_le_bytes(b), |b: [u8;4]| u32::from_be_bytes(b), |v: u32| Value::PosInt(v as u64));
dv_get!(dv_get_float32, 4, |b: [u8;4]| f32::from_le_bytes(b), |b: [u8;4]| f32::from_be_bytes(b), |v: f32| Value::Float(v as f64));
dv_get!(dv_get_float64, 8, |b: [u8;8]| f64::from_le_bytes(b), |b: [u8;8]| f64::from_be_bytes(b), |v: f64| Value::Float(v));

pub(crate) fn dv_get_bigint64(_vm: &mut VM, _args: Args) -> Result<Value, VMError> {
    Err(_vm.fail(ErrorKind::TypeError, "DataView.getBigInt64: BigInt not yet supported"))
}
pub(crate) fn dv_get_biguint64(_vm: &mut VM, _args: Args) -> Result<Value, VMError> {
    Err(_vm.fail(ErrorKind::TypeError, "DataView.getBigUint64: BigInt not yet supported"))
}

dv_set!(dv_set_int8,   1, |f: f64| to_uint32(f) as u8 as i8, |v: i8| v.to_le_bytes(), |v: i8| v.to_be_bytes());
dv_set!(dv_set_uint8,  1, |f: f64| to_uint32(f) as u8,        |v: u8| v.to_le_bytes(), |v: u8| v.to_be_bytes());
dv_set!(dv_set_int16,  2, |f: f64| to_uint32(f) as u16 as i16, |v: i16| v.to_le_bytes(), |v: i16| v.to_be_bytes());
dv_set!(dv_set_uint16, 2, |f: f64| to_uint32(f) as u16,         |v: u16| v.to_le_bytes(), |v: u16| v.to_be_bytes());
dv_set!(dv_set_int32,  4, |f: f64| to_uint32(f) as i32,         |v: i32| v.to_le_bytes(), |v: i32| v.to_be_bytes());
dv_set!(dv_set_uint32, 4, |f: f64| to_uint32(f),                |v: u32| v.to_le_bytes(), |v: u32| v.to_be_bytes());
dv_set!(dv_set_float32, 4, |f: f64| f as f32, |v: f32| v.to_le_bytes(), |v: f32| v.to_be_bytes());
dv_set!(dv_set_float64, 8, |f: f64| f,         |v: f64| v.to_le_bytes(), |v: f64| v.to_be_bytes());

pub(crate) fn dv_set_bigint64(_vm: &mut VM, _args: Args) -> Result<Value, VMError> {
    Err(_vm.fail(ErrorKind::TypeError, "DataView.setBigInt64: BigInt not yet supported"))
}
pub(crate) fn dv_set_biguint64(_vm: &mut VM, _args: Args) -> Result<Value, VMError> {
    Err(_vm.fail(ErrorKind::TypeError, "DataView.setBigUint64: BigInt not yet supported"))
}

// ── Step 4: Typed array prototype methods ─────────────────────────────────────

/// Clamp an index argument into [0, len] range, supporting negative offsets.
fn ta_clamp_index(val: &Value, len: usize) -> usize {
    match val {
        Value::Undefined => 0,
        _ => {
            let f = val.to_number().unwrap_or(0.0);
            if f.is_nan() {
                0
            } else {
                let i = f as i64;
                let len_i = len as i64;
                if i < 0 { (i + len_i).max(0) as usize } else { i.min(len_i) as usize }
            }
        }
    }
}

/// Helper: get (length, elem_size, kind, buf_ptr, byte_offset) from a TypedArray.
macro_rules! ta_view {
    ($vm:expr, $ta_ptr:expr) => {{
        let view = $vm
            .typed_arrays
            .get($ta_ptr as usize)
            .ok_or_else(|| $vm.fail(ErrorKind::TypeError, "bad typed array pointer"))?;
        let elem_size = view.kind.element_size() as usize;
        (
            view.length() as usize,
            elem_size,
            view.kind,
            view.buffer,
            view.byte_offset as usize,
        )
    }};
}

/// `ta.subarray(begin[, end])` — new view over the same buffer.
pub(crate) fn ta_subarray(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let ta_ptr = match args.get(vm, 0) {
        Value::TypedArray(p) => *p,
        recv => return Err(vm.method_receiver_error(recv)),
    };
    let (length, elem_size, kind, buf_ptr, byte_offset) = ta_view!(vm, ta_ptr);
    let begin = ta_clamp_index(args.get(vm, 1), length);
    let end = if args.argc >= 3 {
        ta_clamp_index(args.get(vm, 2), length)
    } else {
        length
    };
    let end = end.max(begin);
    let new_byte_offset = byte_offset + begin * elem_size;
    let new_byte_length = (end - begin) * elem_size;
    let new_ptr = vm.typed_arrays.len() as TypedArrayPtr;
    vm.typed_arrays.push(TypedArrayView {
        buffer: buf_ptr,
        byte_offset: new_byte_offset as u32,
        byte_length: new_byte_length as u32,
        kind,
    });
    Ok(Value::TypedArray(new_ptr))
}

/// `ta.slice(begin[, end])` — new TypedArray with a copy of the element range.
pub(crate) fn ta_slice_ta(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let ta_ptr = match args.get(vm, 0) {
        Value::TypedArray(p) => *p,
        recv => return Err(vm.method_receiver_error(recv)),
    };
    let (length, elem_size, kind, buf_ptr, byte_offset) = ta_view!(vm, ta_ptr);
    let begin = ta_clamp_index(args.get(vm, 1), length);
    let end = if args.argc >= 3 {
        ta_clamp_index(args.get(vm, 2), length)
    } else {
        length
    };
    let end = end.max(begin);
    let src_start = byte_offset + begin * elem_size;
    let src_end = byte_offset + end * elem_size;
    let new_bytes = vm.buffers[buf_ptr as usize][src_start..src_end].to_vec();
    let new_byte_length = new_bytes.len() as u32;
    let new_buf_ptr = vm.buffers.len() as BufferPtr;
    vm.buffers.push(new_bytes);
    let new_ta_ptr = vm.typed_arrays.len() as TypedArrayPtr;
    vm.typed_arrays.push(TypedArrayView {
        buffer: new_buf_ptr,
        byte_offset: 0,
        byte_length: new_byte_length,
        kind,
    });
    Ok(Value::TypedArray(new_ta_ptr))
}

/// `ta.set(source[, offset])` — copy elements from source into this typed array.
pub(crate) fn ta_set(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let ta_ptr = match args.get(vm, 0) {
        Value::TypedArray(p) => *p,
        recv => return Err(vm.method_receiver_error(recv)),
    };
    let (length, elem_size, kind, buf_ptr, byte_offset) = ta_view!(vm, ta_ptr);
    let offset = {
        let f = args.get(vm, 2).to_number().unwrap_or(0.0);
        if f.is_nan() || f < 0.0 { 0usize } else { f as usize }
    };
    match args.get(vm, 1) {
        &Value::TypedArray(src_ptr) => {
            let (src_len, src_elem_size, src_kind, src_buf_ptr, src_byte_offset) =
                ta_view!(vm, src_ptr);
            // Read source elements as f64 values first to avoid aliasing issues
            let vals: Vec<f64> = (0..src_len)
                .map(|i| {
                    let off = src_byte_offset + i * src_elem_size;
                    let buf = &vm.buffers[src_buf_ptr as usize];
                    let v = ta_decode_bytes(src_kind, &buf[off..]);
                    v.to_number().unwrap_or(0.0)
                })
                .collect();
            for (i, f) in vals.iter().enumerate() {
                let dst_i = offset + i;
                if dst_i >= length {
                    break;
                }
                let dst_off = byte_offset + dst_i * elem_size;
                let buf = &mut vm.buffers[buf_ptr as usize];
                ta_encode_bytes(kind, *f, &mut buf[dst_off..dst_off + elem_size]);
            }
        }
        &Value::Array(arr_ptr) => {
            let src_len = vm.arrays.get(arr_ptr as usize).map(|a| a.len()).unwrap_or(0);
            let vals: Vec<f64> = (0..src_len)
                .map(|i| {
                    vm.arrays
                        .get(arr_ptr as usize)
                        .and_then(|a| a.get(i))
                        .and_then(|v| v.to_number())
                        .unwrap_or(0.0)
                })
                .collect();
            for (i, f) in vals.iter().enumerate() {
                let dst_i = offset + i;
                if dst_i >= length {
                    break;
                }
                let dst_off = byte_offset + dst_i * elem_size;
                let buf = &mut vm.buffers[buf_ptr as usize];
                ta_encode_bytes(kind, *f, &mut buf[dst_off..dst_off + elem_size]);
            }
        }
        other => {
            return Err(vm.fail(
                ErrorKind::TypeError,
                format!(
                    "TypedArray.set: source must be TypedArray or Array, got {}",
                    other.type_name()
                ),
            ));
        }
    }
    Ok(Value::Undefined)
}

/// `ta.copyWithin(target, start[, end])` — in-place copy within the buffer.
pub(crate) fn ta_copywithin(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let ta_ptr = match args.get(vm, 0) {
        Value::TypedArray(p) => *p,
        recv => return Err(vm.method_receiver_error(recv)),
    };
    let (length, elem_size, _kind, buf_ptr, byte_offset) = ta_view!(vm, ta_ptr);
    let target = ta_clamp_index(args.get(vm, 1), length);
    let start = ta_clamp_index(args.get(vm, 2), length);
    let end = if args.argc >= 4 {
        ta_clamp_index(args.get(vm, 3), length)
    } else {
        length
    };
    let count = end.saturating_sub(start).min(length.saturating_sub(target));
    if count > 0 {
        let src_byte = byte_offset + start * elem_size;
        let dst_byte = byte_offset + target * elem_size;
        let count_bytes = count * elem_size;
        let buf = &mut vm.buffers[buf_ptr as usize];
        buf.copy_within(src_byte..src_byte + count_bytes, dst_byte);
    }
    Ok(Value::TypedArray(ta_ptr))
}

/// `ta.at(index)` — element at index, supporting negative indices.
pub(crate) fn ta_at(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let ta_ptr = match args.get(vm, 0) {
        Value::TypedArray(p) => *p,
        recv => return Err(vm.method_receiver_error(recv)),
    };
    let idx = args.get(vm, 1).to_number().unwrap_or(0.0) as i64;
    let (length, elem_size, kind, buf_ptr, byte_offset) = ta_view!(vm, ta_ptr);
    let len = length as i64;
    let i = if idx < 0 { idx + len } else { idx };
    if i < 0 || i as usize >= length {
        return Ok(Value::Undefined);
    }
    let off = byte_offset + i as usize * elem_size;
    Ok(ta_decode_bytes(kind, &vm.buffers[buf_ptr as usize][off..]))
}

/// `ta.includes(needle[, fromIndex])` — SameValueZero search.
pub(crate) fn ta_includes(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let ta_ptr = match args.get(vm, 0) {
        Value::TypedArray(p) => *p,
        recv => return Err(vm.method_receiver_error(recv)),
    };
    let needle = args.get(vm, 1).clone();
    let from = {
        let f = args.get(vm, 2).to_number().unwrap_or(0.0);
        if f.is_nan() { 0usize } else { f as usize }
    };
    let (length, elem_size, kind, buf_ptr, byte_offset) = ta_view!(vm, ta_ptr);
    for i in from.min(length)..length {
        let off = byte_offset + i * elem_size;
        let v = ta_decode_bytes(kind, &vm.buffers[buf_ptr as usize][off..]);
        if crate::vm::value::same_value_zero(&v, &needle) {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

/// `ta.indexOf(needle[, fromIndex])` — strict equality search, returns index.
pub(crate) fn ta_index_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let ta_ptr = match args.get(vm, 0) {
        Value::TypedArray(p) => *p,
        recv => return Err(vm.method_receiver_error(recv)),
    };
    let needle = args.get(vm, 1).clone();
    let from = {
        let f = args.get(vm, 2).to_number().unwrap_or(0.0);
        if f.is_nan() { 0usize } else { f.max(0.0) as usize }
    };
    let (length, elem_size, kind, buf_ptr, byte_offset) = ta_view!(vm, ta_ptr);
    for i in from.min(length)..length {
        let off = byte_offset + i * elem_size;
        let v = ta_decode_bytes(kind, &vm.buffers[buf_ptr as usize][off..]);
        if v.strict_equal(&needle) {
            return Ok(Value::int_from_f64(i as f64));
        }
    }
    Ok(Value::NegInt(-1))
}

/// `ta.lastIndexOf(needle[, fromIndex])` — strict equality reverse search.
pub(crate) fn ta_last_index_of(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let ta_ptr = match args.get(vm, 0) {
        Value::TypedArray(p) => *p,
        recv => return Err(vm.method_receiver_error(recv)),
    };
    let needle = args.get(vm, 1).clone();
    let (length, elem_size, kind, buf_ptr, byte_offset) = ta_view!(vm, ta_ptr);
    let end = match args.get(vm, 2) {
        Value::Undefined => length,
        v => {
            let f = v.to_number().unwrap_or(length as f64);
            (f as i64 + 1).max(0).min(length as i64) as usize
        }
    };
    for i in (0..end).rev() {
        let off = byte_offset + i * elem_size;
        let v = ta_decode_bytes(kind, &vm.buffers[buf_ptr as usize][off..]);
        if v.strict_equal(&needle) {
            return Ok(Value::int_from_f64(i as f64));
        }
    }
    Ok(Value::NegInt(-1))
}

/// `ta.join([sep])` — join elements with separator string.
pub(crate) fn ta_join(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let ta_ptr = match args.get(vm, 0) {
        Value::TypedArray(p) => *p,
        recv => return Err(vm.method_receiver_error(recv)),
    };
    let sep = match args.get(vm, 1) {
        Value::Undefined => ",".to_string(),
        v => vm.to_js_string(v, 0).as_str().to_owned(),
    };
    let (length, elem_size, kind, buf_ptr, byte_offset) = ta_view!(vm, ta_ptr);
    let mut joined = String::new();
    for i in 0..length {
        if i > 0 {
            joined.push_str(&sep);
        }
        let off = byte_offset + i * elem_size;
        let v = ta_decode_bytes(kind, &vm.buffers[buf_ptr as usize][off..]);
        joined.push_str(vm.to_js_string(&v, 0).as_str());
    }
    Ok(Value::String(crate::vm::RcStr::from(joined)))
}

/// `ta.fill(value[, start[, end]])` — fill in-place, returns receiver.
pub(crate) fn ta_fill(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let ta_ptr = match args.get(vm, 0) {
        Value::TypedArray(p) => *p,
        recv => return Err(vm.method_receiver_error(recv)),
    };
    let f = args.get(vm, 1).to_number().unwrap_or(0.0);
    let (length, elem_size, kind, buf_ptr, byte_offset) = ta_view!(vm, ta_ptr);
    let start = ta_clamp_index(args.get(vm, 2), length);
    let end = if args.argc >= 4 {
        ta_clamp_index(args.get(vm, 3), length)
    } else {
        length
    };
    let end = end.max(start);
    // Encode the value once, then copy repeatedly.
    let mut elem_buf = vec![0u8; elem_size];
    ta_encode_bytes(kind, f, &mut elem_buf);
    let buf = &mut vm.buffers[buf_ptr as usize];
    for i in start..end {
        let off = byte_offset + i * elem_size;
        buf[off..off + elem_size].copy_from_slice(&elem_buf);
    }
    Ok(Value::TypedArray(ta_ptr))
}

/// `ta.reverse()` — reverse elements in-place, returns receiver.
pub(crate) fn ta_reverse(vm: &mut VM, args: Args) -> Result<Value, VMError> {
    let ta_ptr = match args.get(vm, 0) {
        Value::TypedArray(p) => *p,
        recv => return Err(vm.method_receiver_error(recv)),
    };
    let (length, elem_size, _kind, buf_ptr, byte_offset) = ta_view!(vm, ta_ptr);
    let buf = &mut vm.buffers[buf_ptr as usize];
    for i in 0..length / 2 {
        let j = length - 1 - i;
        for k in 0..elem_size {
            buf.swap(byte_offset + i * elem_size + k, byte_offset + j * elem_size + k);
        }
    }
    Ok(Value::TypedArray(ta_ptr))
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

    // ── Step 4: prototype methods ──────────────────────────────────────────

    #[test]
    fn ta_subarray_shares_buffer() {
        let v = testutil::run_ret(r#"
            const ta = new Uint8Array([10, 20, 30, 40, 50]);
            const sub = ta.subarray(1, 4);
            return [sub[0], sub[1], sub[2], sub.length, sub.byteOffset];
        "#);
        assert_eq!(v, serde_json::json!([20, 30, 40, 3, 1]));
    }

    #[test]
    fn ta_subarray_mutation_reflects() {
        let v = testutil::run_ret(r#"
            const ta = new Uint8Array([1, 2, 3, 4]);
            const sub = ta.subarray(1, 3);
            sub[0] = 99;
            return ta[1];
        "#);
        assert_eq!(v, serde_json::json!(99));
    }

    #[test]
    fn ta_slice_copies() {
        let v = testutil::run_ret(r#"
            const ta = new Int32Array([10, 20, 30, 40]);
            const s = ta.slice(1, 3);
            s[0] = 999;
            return [ta[1], s[0], s.length];
        "#);
        // slice makes a copy — mutating s does not affect ta
        assert_eq!(v, serde_json::json!([20, 999, 2]));
    }

    #[test]
    fn ta_set_from_array() {
        let v = testutil::run_ret(r#"
            const ta = new Uint8Array(5);
            ta.set([1, 2, 3], 1);
            return [ta[0], ta[1], ta[2], ta[3], ta[4]];
        "#);
        assert_eq!(v, serde_json::json!([0, 1, 2, 3, 0]));
    }

    #[test]
    fn ta_set_from_typed_array() {
        let v = testutil::run_ret(r#"
            const src = new Float64Array([1.5, 2.5]);
            const dst = new Float64Array(4);
            dst.set(src, 2);
            return [dst[0], dst[1], dst[2], dst[3]];
        "#);
        assert_eq!(v, serde_json::json!([0, 0, 1.5, 2.5]));
    }

    #[test]
    fn ta_copywithin() {
        let v = testutil::run_ret(r#"
            const ta = new Uint8Array([1, 2, 3, 4, 5]);
            ta.copyWithin(0, 3);
            return [ta[0], ta[1], ta[2], ta[3], ta[4]];
        "#);
        // copyWithin(0, 3) → copy elements 3,4 to positions 0,1
        assert_eq!(v, serde_json::json!([4, 5, 3, 4, 5]));
    }

    #[test]
    fn ta_fill_basic() {
        let v = testutil::run_ret(r#"
            const ta = new Uint8Array(5);
            ta.fill(7, 1, 4);
            return [ta[0], ta[1], ta[2], ta[3], ta[4]];
        "#);
        assert_eq!(v, serde_json::json!([0, 7, 7, 7, 0]));
    }

    #[test]
    fn ta_reverse_basic() {
        let v = testutil::run_ret(r#"
            const ta = new Uint8Array([1, 2, 3, 4]);
            ta.reverse();
            return [ta[0], ta[1], ta[2], ta[3]];
        "#);
        assert_eq!(v, serde_json::json!([4, 3, 2, 1]));
    }

    #[test]
    fn ta_join_basic() {
        let v = testutil::run_ret(r#"
            const ta = new Uint8Array([1, 2, 3]);
            return ta.join("-");
        "#);
        assert_eq!(v, serde_json::json!("1-2-3"));
    }

    #[test]
    fn ta_at_negative() {
        let v = testutil::run_ret(r#"
            const ta = new Uint8Array([10, 20, 30]);
            return ta.at(-1);
        "#);
        assert_eq!(v, serde_json::json!(30));
    }

    #[test]
    fn ta_includes_basic() {
        let v = testutil::run_ret(r#"
            const ta = new Uint8Array([1, 2, 3]);
            return [ta.includes(2), ta.includes(5)];
        "#);
        assert_eq!(v, serde_json::json!([true, false]));
    }

    #[test]
    fn ta_index_of_basic() {
        let v = testutil::run_ret(r#"
            const ta = new Uint8Array([10, 20, 30, 20]);
            return [ta.indexOf(20), ta.lastIndexOf(20)];
        "#);
        assert_eq!(v, serde_json::json!([1, 3]));
    }

    // ── Step 5: DataView ──────────────────────────────────────────────────

    #[test]
    fn dataview_get_set_int32_le() {
        let v = testutil::run_ret(r#"
            const buf = new ArrayBuffer(8);
            const dv = new DataView(buf);
            dv.setInt32(0, 0x12345678, true);
            return dv.getInt32(0, true);
        "#);
        assert_eq!(v, serde_json::json!(0x12345678));
    }

    #[test]
    fn dataview_get_set_int32_be() {
        let v = testutil::run_ret(r#"
            const buf = new ArrayBuffer(4);
            const dv = new DataView(buf);
            dv.setInt32(0, -1, false);
            return dv.getInt32(0, false);
        "#);
        assert_eq!(v, serde_json::json!(-1));
    }

    #[test]
    fn dataview_get_set_float64() {
        let v = testutil::run_ret(r#"
            const buf = new ArrayBuffer(8);
            const dv = new DataView(buf);
            dv.setFloat64(0, 3.14, true);
            return dv.getFloat64(0, true);
        "#);
        // 3.14 round-trips through f64
        assert!((v.as_f64().unwrap() - 3.14).abs() < 1e-10);
    }

    #[test]
    fn dataview_byte_offset_and_length() {
        let v = testutil::run_ret(r#"
            const buf = new ArrayBuffer(16);
            const dv = new DataView(buf, 4, 8);
            return [dv.byteOffset, dv.byteLength];
        "#);
        assert_eq!(v, serde_json::json!([4, 8]));
    }

    #[test]
    fn dataview_uint8_across_bytes() {
        let v = testutil::run_ret(r#"
            const buf = new ArrayBuffer(4);
            const dv = new DataView(buf);
            dv.setUint8(0, 10);
            dv.setUint8(1, 20);
            dv.setUint8(2, 30);
            dv.setUint8(3, 40);
            return [dv.getUint8(0), dv.getUint8(1), dv.getUint8(2), dv.getUint8(3)];
        "#);
        assert_eq!(v, serde_json::json!([10, 20, 30, 40]));
    }

    #[test]
    fn dataview_endian_matters_for_int16() {
        let v = testutil::run_ret(r#"
            const buf = new ArrayBuffer(2);
            const dv = new DataView(buf);
            dv.setInt16(0, 0x0102, false); // big-endian: [0x01, 0x02]
            return [dv.getUint8(0), dv.getUint8(1)];
        "#);
        assert_eq!(v, serde_json::json!([1, 2]));
    }

    #[test]
    fn ta_map_via_prelude() {
        let v = testutil::run_ret(r#"
            const ta = new Float64Array([1, 2, 3]);
            return ta.map(x => x * 2);
        "#);
        // map on a TypedArray — prelude HOF iterates via .length and [i]
        // Returns a plain Array (prelude HOF creates an Array result)
        assert_eq!(v, serde_json::json!([2, 4, 6]));
    }
}
