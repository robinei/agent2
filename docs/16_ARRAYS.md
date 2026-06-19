# Phase 16 — Typed arrays & binary data

Add `ArrayBuffer`, `DataView`, and all 9 typed-array constructors
(`Float64Array`, `Int32Array`, `Uint8Array`, …) — the largest *single-name*
compat gap after `PushName` landed. The 15_COMPAT Step 3a histogram
recorded `Float64Array` as the #1 unresolved `ReferenceError` bucket
(1,058 tests), `ArrayBuffer` as #7 (395), and `DataView` at #14 (92).

Together they let ~1,500 test262 tests **reach execution** — but reaching
execution is not passing. Many typed-array tests gate on a *second*
out-of-scope feature (`Symbol.iterator` / well-known symbols, property
descriptors, the iterator protocol, `%TypedArray%` intrinsic identity,
detached-buffer semantics) and will re-fail there. Treat the bucket
counts as an upper bound on *unblocking*, not a pass projection; the
true yield is whatever each step's rebaseline reports.

**Sequencing caveat.** `Symbol` (857) is a comparable bucket *and* a
likely prerequisite that unlocks part of this one (typed-array iteration,
`Symbol.toStringTag`, species checks). Doing a minimal well-known-symbols
pass first may yield more total passes per unit effort than typed arrays
in isolation. Picking typed arrays first is defensible on single-bucket
size, but validate that assumption against the corpus before committing
all 8 steps.

**Scope boundary.** This phase adds the buffer/view/subarray surface
*without* iterators, detachable buffers, or the `SharedArrayBuffer`/
`Atomics` / resizable-buffer proposals — those stay in 4_FUTURE and in
15_COMPAT's permanent skip list. Getter/setter descriptors and
`Symbol.species` are excluded. Higher-order typed-array methods (`map`,
`filter`, …) reuse the prelude lowering exactly as ordinary arrays do;
no iterator protocol.

## Ground rules

- Commit per step, `typedarrays:` prefix.
- Gate: `cargo fmt && cargo clippy && cargo test` — all three green, plus
  a rebaseline `cargo run -p conformance` showing the new pass count.
- Every new `Value` variant, `TypeTag`, and heap enters the same
  machinery its peers already use: `Value::type_name`,
  `same_value_zero`, `strict_equal`, `is_truthy`, `to_number`,
  `compare`, `stack_value_to_json` (rejection), the `builtins!`
  macro, `TypeCheck` / prototype walk, and the optimizer's purity tables.
- **No new instructions.** All typed-array element access routes through
  the existing `get_property` / `set_property` canonical ladder
  (the integer-key fast path and `named_get_property` /
  `named_set_property` — `dispatch.rs:39` / `:299`). The `IndexGet`
  and `IndexSet` instructions are thin adapters that call these; they
  need zero changes. The `GetLength` instruction gets one new arm.
- Typed arrays / buffers / views have **no JSON form** (guardrail:
  `15_COMPAT.md`). Reject them in `stack_value_to_json` exactly like
  `Closure`/`Promise`/`RegExp` — with a test per variant.
- All new signatures match the JS contract. When unsure, verify with
  `node -e`.
- `14_CLEANUP.md`'s lint rules still apply.

## Design (read before the steps)

### Buffer ownership

One `Vec<u8>` per `ArrayBuffer`. Buffers are owned by the heap,
never aliased — the typed array and DataView lifetime is just
outliving the VM, which never reclaims heaps. A view holds a
`BufferPtr` (index into `buffers`) plus a `byte_offset` and
`byte_length`; reading/writing calculates the byte range and
slurps/emits bytes through a per-element-kind codec into/from
`Value`. Byte alignment is checked at construction time; an
unaligned access at read/write time is an opaque
"detached buffer on view access" (`TypeError`), matching JS.

### Value enum: three new variants

| Variant | Payload | Heap |
|---------|---------|------|
| `Value::ArrayBuffer(BufferPtr)` | u32 → `buffers` | `Vec<Vec<u8>>` |
| `Value::TypedArray(TypedArrayPtr)` | u32 → `typed_arrays` | `Vec<TypedArrayView>` |
| `Value::DataView(DataViewPtr)` | u32 → `data_views` | `Vec<DataViewEntry>` |

All three fit in 16 bytes (discriminant + u32 payload). The new
heaps are grow-only `Vec`s, same as `arrays`/`objects`/`maps`/`sets`.

### TypedArrayView (one struct, nine element kinds)

```rust
pub struct TypedArrayView {
    pub buffer: BufferPtr,
    pub byte_offset: u32,
    pub byte_length: u32,
    pub kind: TypedArrayKind,
}

pub enum TypedArrayKind {
    Int8, Uint8, Uint8Clamped,
    Int16, Uint16,
    Int32, Uint32,
    Float32, Float64,
    BigInt64, BigUint64,
}
```

`byte_length` is always a multiple of the element size and is set at
construction. The JS-visible `.length` is `byte_length / element_size`;
`IndexGet`/`IndexSet` convert a numeric index to `byte_offset + index *
element_size` and read/write that byte range.

### DataViewEntry (method-based access, no `[i]`)

```rust
pub struct DataViewEntry {
    pub buffer: BufferPtr,
    pub byte_offset: u32,
    pub byte_length: u32,
}
```

DataView has no `[i]` access — all reads and writes go through
builtin methods (`getInt8(offset)`, `setFloat64(offset, value)`, …).

### TypeTag expansion

Current: Array, Object, Map, Set, RegExp, Function, String, Number, Boolean (9).
Add: ArrayBuffer, Int8Array, Uint8Array, Uint8ClampedArray, Int16Array,
Uint16Array, Int32Array, Uint32Array, Float32Array, Float64Array,
BigInt64Array, BigUint64Array, DataView (13 new → 22 total).

Each maps a JS constructor with its own `.prototype` and `x instanceof
Float64Array` chain.

### Constructor / builtin registration

All typed arrays and ArrayBuffer/DataView are `BuiltinKind::Constructor
{ type_tag }` in the `builtins!` macro. Their string-keyed name is the
constructor's JS name (e.g. `"Float64Array"`, `"ArrayBuffer"`). The
`for_constructor` lookup resolves bare identifiers through `PushName`;
`Instr::New` dispatches through `construct_builtin`. Static properties
(`Float64Array.BYTES_PER_ELEMENT`, `ArrayBuffer.isView`) live as
virtual rungs read by the compiler (like `Array.isArray` today).

### Property access: integration into the canonical ladder

All property access routes through `get_property` / `set_property`
(`dispatch.rs:39` / `:299`) — the `IndexGet` and `IndexSet`
instructions are thin adapters that hand off immediately. Typed array
element access must integrate into the same ladder, not bypass it.
The specific integration points are:

**Read path — `get_property`** (integer-key fast path, line 69):
Add a `Value::TypedArray(p)` arm alongside the existing `Array` and
`String` arms. At index `i`, bounds-check against `view.length()`,
read `element_size` bytes from `buffer[byte_offset + i * elem_size]`,
decode bytes → `Value::Float(f64)` (or `PosInt` for integer types),
return. OOB → `Undefined` (JS-faithful).

**Read path — `named_get_property`** (line 120):
Add arms for `Value::TypedArray`, `Value::ArrayBuffer`, and
`Value::DataView` with virtual rungs then fallback to
`type_proto_lookup`:

| Receiver | Virtual rungs | TypeTag for proto |
|----------|--------------|-------------------|
| TypedArray | `"length"`, `"byteLength"`, `"byteOffset"`, `"buffer"`, `"BYTES_PER_ELEMENT"` (for constructor reads) | The concrete `TypeTag` (`Float64`, `Int32`, …) |
| ArrayBuffer | `"byteLength"` | `ArrayBuffer` |
| DataView | `"byteLength"`, `"byteOffset"`, `"buffer"` | `DataView` |

**Write path — `set_property`** (integer-key fast path, line 307):
Add a `Value::TypedArray(p)` arm. **Do not copy the Array arm's
error-on-OOB behavior** — typed arrays are integer-indexed exotic
objects, and their `[[Set]]` *silently discards* writes to any index
that is out of bounds or negative (no error, no property created),
even in strict mode. So: if `0 <= i < view.length()`, encode `val` to
bytes (ToNumber + clamp/truncate per kind, after reading the old value
for `SetMode::Old`) and write; **otherwise the write is a no-op** —
return per `SetMode` (the assigned `val` for `SetMode::New`, `undefined`
for `SetMode::Old`) without touching the buffer. This mirrors JS, where
`ta[ta.length] = x` followed by `ta[ta.length]` yields `undefined`, and
test262 asserts exactly that. `ArrayBuffer` and `DataView` have no
integer-key path — they fall through to `named_set_property`.

**Write path — `named_set_property`** (line 344):
Add arms for `Value::TypedArray`, `Value::ArrayBuffer`,
`Value::DataView` — all → `TypeError` (non-extensible, matching
the existing `Map`/`Set`/`Array` arms at line 489+).

**`GetLength` instruction**: add `Value::TypedArray` arm →
`view.byte_length / view.kind.element_size()`. This is the
compiler fast path (emitted for `.length` on typed arrays, same
as it does for ordinary arrays today). The reflective path
(`ta["length"]` via `named_get_property`) delegates to the same
virtual rung above.

**`method_for_receiver`** (generated by `builtins!`): add new
type columns `typedarray` and `dataview` for method resolution.
A `typedarray: true` row on a method means it resolves for all
9 kind-tagged typed arrays; `dataview: true` resolves for DataView
receivers only.

**`type_proto_lookup`**: already parameterized on `TypeTag` — the
new tags Just Work. Each concrete typed array tag chains to a shared
`TypedArray` prototype intermediate (see §Prototype chain below).

### Higher-order methods

The prelude `HOFS` table (`interp/src/prelude.rs`) already lowers
`arr.map(cb)` to a helper that iterates via `a.length` and `a[i]`.
Typed arrays have `.length` and support `[i]` access — so the
helpers work **unchanged** when extended to typed array receivers.
The compiler's `compile_method_call` just needs to recognize typed
array method names in the same lowering path.

No per-method typed-array handler differences are needed; `subarray`,
`slice`, `set`, `fill`, `copyWithin`, `join`, `reverse`, `indexOf`,
`lastIndexOf`, `includes`, `at` are added as native builtin
handlers. `fill`, `join`, `reverse`, `indexOf`, `lastIndexOf`,
`includes`, `at` **delegate to the existing array-handler code**
where possible (they share the same semantics — just the element
type differs at read/write time).

### Prototype chain

```
Object.prototype
  ↑
TypedArray.prototype   ← all 9 typed arrays' .prototype chains here
  ↑
Float64Array.prototype / Int32Array.prototype / …
  ↑
float64_instance / int32_instance
```

`TypedArray` is not a `TypeTag`; it is a **prototype intermediate
object** allocated as a plain `Object` with `proto: ObjectProto`. The
`%TypedArray%.prototype` methods live on this intermediate object
as builtin entries (not a separate builtin namespace). Each concrete
typed array `.prototype` chains to it.

### Byte order

The underlying buffer is host byte order (little-endian on x86/ARM).
Typed arrays read/write in native byte order by default; the JS spec
uses **host-native** byte order for the typed views (not little-endian
per se) and lets DataView pick. Matching JS exactly: the `TypedArray`
views **always** use host-native byte order — which on the x86/ARM
targets *is* little-endian, so the observable bytes agree with every
mainstream engine — and DataView get/set methods use
`from_le_bytes`/`from_be_bytes` per a `littleEndian` boolean parameter
(default `false` = big-endian).

---

## Step 1: Substrate — heaps, types, Value variants, TypeTags

Add the three heaps (`buffers`, `typed_arrays`, `data_views` to `VM`),
the three `Value` variants, the 13 `TypeTag` entries, `TypedArrayKind`,
`TypedArrayView`, and `DataViewEntry`. Wire `Value::type_name`
(`"arraybuffer"`, `"object"` for typed arrays/DataView — matching JS
`typeof`), `strict_equal` (pointer identity), `same_value_zero`,
`is_truthy` (always `true` like arrays), `to_number` (→ `None`),
`compare` (→ `None`). Add the `stack_value_to_json` rejection per
variant. Extend `TypeTag::ALL` and `COUNT`. Wire `TypeTag::name` for
each new tag. **No new instructions or builtins yet** — just the shapes
and boilerplate so later steps can allocate and manipulate.

- [ ] `VM::buffers`, `VM::typed_arrays`, `VM::data_views` — three
      grow-only `Vec`s, initialized empty in `VM::new`.
- [ ] `Value::ArrayBuffer(BufferPtr)`, `Value::TypedArray(TypedArrayPtr)`,
      `Value::DataView(DataViewPtr)` — three new enum variants.
- [ ] `TypedArrayKind` enum (9 variants), `TypedArrayView` struct,
      `DataViewEntry` struct.
- [ ] `TypeTag` 13 new variants, `ALL` array + `COUNT`, `name()`.
- [ ] Wire all `Value` methods (type_name, strict_equal,
      same_value_zero, is_truthy, to_number, compare).
- [ ] **Audit `_ =>` fallback arms.** The exhaustive matches above force
      the new variants to be handled; the danger is the *non*-exhaustive
      sites — `_ => false` / `_ => None` / `_ => …object…` arms in
      `value.rs` and especially the property ladder in `dispatch.rs` — that
      would silently swallow the new variants with object-like (or wrong)
      behavior instead of failing to compile. Grep every `_ =>` arm that
      can receive a `Value` and confirm the new variants land somewhere
      correct (or are explicitly listed). This is the only place a new
      heap variant leaks unnoticed.
- [ ] `stack_value_to_json` rejects: `ArrayBuffer`/`TypedArray`/`DataView`.
- [ ] Optimizer: classify new instructions (Step 4) as not pure
      (reads/writes mutable buffer; heap state visible).
- [ ] Compile: `cargo test` passes on existing suite.
- [ ] Acceptance: `Value` tests for type_name, strict_equal,
      is_truthy, to_number → None, JSON rejection per variant.
      No conformance change (nothing allocated yet).

## Step 2: ArrayBuffer

`ArrayBuffer` constructor + `ArrayBuffer.isView` + `ArrayBuffer.prototype`.
ByteLength is a named virtual rung (like `.length` on arrays). One method:
`slice(begin[, end])` → new buffer with a copy of the byte range.

- [ ] Constructor row in `builtins!`:
      `ArrayBufferCtor, BuiltinKind::Constructor { type_tag: TypeTag::ArrayBuffer }, "ArrayBuffer", 1, 1, ...`
      Handler: `arraybuffer_ctor(n)` → allocate `buffers` entry with
      `vec![0u8; n]`, push `Value::ArrayBuffer(ptr)`. Length goes through
      JS `ToIndex`: a fractional value is **truncated** (`new ArrayBuffer(1.5)`
      → length 1, *not* an error); only a negative value or one exceeding
      the index range → `RangeError` mapped to `ValueError`.
- [ ] `ByteLength`: virtual rung (compiler reads `.byteLength` off
      ArrayBuffer values; the VM resolves through a new function
      `vm.get_buffer_byte_length(ptr)` that `ObjGet`/`GetLength` dispatch to).
      Or: use the existing `GetLength` instruction for array-like types
      by mapping `Value::ArrayBuffer` → byte_length. (Simplest: add
      one arm to `GetLength` dispatch in `methods.rs`.)
- [ ] `slice(begin, end)`: allocator that copies the byte range into a
      new buffer.
- [ ] `ArrayBuffer.isView`: static method (`BuiltinKind::Namespace("ArrayBuffer")`).
      Returns `true` for `Value::TypedArray` or `Value::DataView`,
      `false` otherwise.
- [ ] Lazy `ArrayBuffer.prototype` allocation in `prototype_for`. Chain:
      `ArrayBuffer.prototype` → `Object.prototype` → `null`.
- [ ] Acceptance: compile `new ArrayBuffer(16)` / `.byteLength` /
      `ArrayBuffer.isView(ta)` / `.slice()` tests. Check JSON rejection.
      Rebaseline: ~395 fewer `ReferenceError` failures,
      ~50 new passes from buffer-allocation tests.

## Step 3: Typed array constructors + IndexGet/IndexSet

Register all 9 constructors in `builtins!` with their own `TypeTag`.
Each constructor `new T(n)` / `T(n)` allocates an `ArrayBuffer` of
`n * element_size` bytes and a `TypedArrayView` wrapping it at
byte_offset 0. Extend `IndexGet` / `IndexSet` dispatch to
`Value::TypedArray`.

### 3a: Element codec

A free function `typed_array_read(vm, view, index) -> Value` and
`typed_array_write(vm, view, index, value) -> Result<(), VMError>`:

- Read: bounds-check `index < view.length()`, read
  `buffer[byte_offset + index * elem_size .. + elem_size]`, decode
  bytes → `Value::Float(f64)` / `Value::PosInt(u64)` / (future:
  `Value::BigInt` for BigInt64/BigUint64).
  - `BigInt` types: when they can't be represented as `Value`,
    error with `TypeError("BigInt not yet supported")` — pin in
    the divergence ledger.
- Write: bounds-check, `value.to_number()` → f64, clamp/truncate per
  kind (e.g. `Uint8Clamped` → round ties to even, clamp 0..255),
  encode bytes into buffer.
- `Uint8Clamped` takes `value.to_number()`, then:
  `round(x)` → if `<0` → 0, if `>255` → 255, else saturated cast.

### 3b: Constructors

9 handler functions, one macro-generated row per constructor in
`builtins!`. The argument dispatch matches JS, switching on the first
argument's type:

**Numeric-length form** `new T(n)`:
1. `n = max(0, trunc(n))`.
2. `elem_size = kind.element_size()` (1, 2, 4, or 8 bytes).
3. Allocate buffer `vec![0u8; n * elem_size]`.
4. Allocate `TypedArrayView { buffer, byte_offset: 0, byte_length: n * elem_size, kind }`.
5. Return `Value::TypedArray(ptr)`.

**Array / typed-array source form** `new T(arr)` / `new T(otherTyped)`
— **required here, not deferred**: `new Float64Array([1, 2, 3])` is one
of the most common shapes in the corpus (and the Step 6 acceptance test
`new Float64Array([3,1,2]).map(...)` depends on it). When the first arg
is an ordinary `Array` or another `TypedArray`:
1. `len = source.length`; allocate buffer `vec![0u8; len * elem_size]`.
2. For each `i`, read `source[i]` (typed-array read, or `arr[i]` for a
   plain array), `to_number()`, and write via `typed_array_write` —
   reusing the element codec from 3a.
3. Return `Value::TypedArray(ptr)`.

The general iterable form (`new T(iterable)` over an arbitrary
iterator) stays out of scope with the rest of the iterator protocol —
plain `Array` and `TypedArray` sources cover the bulk of the corpus.

The `new T(buffer[, byteOffset[, length]])` ArrayBuffer overload (shared
backing store, no copy) lands later in Step 7a.

### 3c: Integration into get_property / set_property

The canonical property ladder (`get_property` / `set_property` in
`dispatch.rs`) is the single integration point. `IndexGet` and
`IndexSet` are thin adapters that call these; they need no changes.

**`get_property` integer-key fast path** (dispatch.rs:71):
Add `Value::TypedArray(p)` arm alongside `Array` and `String`.
At idx `i`: bounds-check `i < view.length()`, read
`buffer[byte_offset + i*elem_size .. +elem_size]`, decode bytes →
`Value::Float(f64)` or `Value::PosInt`. OOB **or negative → `Undefined`**
(integer-indexed exotic `[[Get]]`, *not* the Array/string arms'
negative-index `ValueError`).

**`named_get_property`** (dispatch.rs:120):
Add receiver arms for `Value::TypedArray`, `Value::ArrayBuffer`,
`Value::DataView`. Each arm has virtual rungs (`.length`,
`.byteLength`, `.byteOffset`, `.buffer`, `.BYTES_PER_ELEMENT` as
appropriate per type) then falls through to
`type_proto_lookup(concrete_type_tag, field, receiver)`.

**`set_property` integer-key fast path** (dispatch.rs:307):
Add `Value::TypedArray(p)` arm. In-bounds (`0 <= i < length`): read old
value for `SetMode::Old`, encode `val` via `typed_array_write`, write
buffer, return per `SetMode`. **Negative/OOB → silent no-op** (integer-
indexed exotic `[[Set]]` discards the write — *not* the Array arm's
`ValueError`); still return per `SetMode` without touching the buffer.

**`named_set_property`** (dispatch.rs:344):
Add arms for `Value::TypedArray`, `Value::ArrayBuffer`,
`Value::DataView` → `TypeError` (non-extensible, matching the
existing `Map`/`Set`/`Array` rejection arms).

**`GetLength` instruction**: add `Value::TypedArray` arm →
`view.byte_length / view.kind.element_size()`.

- [ ] `typed_array_read` / `typed_array_write` with all 9 kinds.
- [ ] 9 constructor builtin rows, each handling both the numeric-length
      and the array/typed-array source forms (3b).
- [ ] Integrate into `get_property` / `set_property` / `named_get_property` /
      `named_set_property` / `GetLength` — all 5 dispatch points above.
- [ ] Typed array `.prototype` chain per type.
- [ ] `BYTES_PER_ELEMENT` virtual rung per constructor.
- [ ] `typeof` → `"object"` for all typed array values.
- [ ] Acceptance: compile `new Float64Array(4)` / `new Float64Array([1,2,3])`
      (array-source form) / `ta[0] = 1.5` / `ta[1]` / `ta.length` /
      `ta.byteLength`. Tests per element kind covering boundary values
      (NaN, ±Infinity, min/max per type, overflow wrap for integer types).
      **OOB/negative semantics:** `ta[ta.length] = 9; ta[ta.length] === undefined`
      (write dropped), `ta[-1] === undefined`, `ta[5]` on a length-4 array
      `=== undefined` — no error on any of these.
- [ ] Rebaseline: realistically **well under** the 1,058 `Float64Array`
      bucket converts to *passes* — most of that bucket reaches execution
      but then re-fails on a second out-of-scope feature (`Symbol.iterator`,
      property descriptors, `%TypedArray%` intrinsic identity, detach
      semantics). Expect a few hundred passes plus a large shift of the
      bucket into new, more-specific runtime-error buckets; record the
      actual number from the rebaseline rather than projecting it.

## Step 4: Typed array prototype methods

Methods that are specific to typed arrays (subarray, set, slice,
copyWithin) and the polymorphic methods that share semantics with
ordinary arrays (fill, join, reverse, indexOf, lastIndexOf, includes,
at). Where possible, delegate to the existing array-handler code.

### 4a: subarray(begin[, end])

Returns a new `TypedArrayView` wrapping the **same buffer** at a
different `byte_offset` / `byte_length`. Builtin handler:

1. Compute `begin`/`end` as indices (clamped to `[0, length]`).
2. `byte_offset = view.byte_offset + begin * elem_size`.
3. `byte_length = (end - begin) * elem_size`.
4. Allocate new `TypedArrayView` with same buffer/kind, new offsets.
5. Return `Value::TypedArray(new_ptr)`.

### 4b: set(source[, offset])

Copies elements from `source` (a typed array or ordinary array) into
`this` at `offset`. JS: interleaved read-before-write semantics for
overlapping buffers — but this VM doesn't worry about overlapping
views (a single owner; the buffer is behind a `Vec<u8>` and both views
index the same `Vec`, so an overlapping copy must handle forward/
reverse iteration to avoid clobbering). Builtin handler:

1. If source is `TypedArray`: iterate, read value from source, write to self.
2. If source is ordinary `Array`: iterate, `value.to_number()`, write to self.
3. If source is anything else: `TypeError`.

### 4c: slice(begin[, end])

Like `subarray` but returns a **new buffer** with a copy of the bytes.

### 4d: copyWithin(target, start[, end])

In-place copy within the typed array. Same tricky overlap logic as
`set` but within the same view. Follow JS: determine copy direction
based on target vs start.

### 4e: fill / join / reverse / indexOf / lastIndexOf / includes / at

Polymorphic: add typed-array receiver handling to the existing handler
functions. Each needs to read elements via `typed_array_read` instead
of `arr[i].clone()`. Factor out the core logic into a helper that
takes a closure/iterator over elements.

- [ ] `subarray` builtin handler + tests.
- [ ] `set` builtin handler (typed array + array sources) + tests.
- [ ] `slice` builtin handler + tests.
- [ ] `copyWithin` builtin handler + tests.
- [ ] `fill` handler extended for typed arrays.
- [ ] `join` handler extended for typed arrays.
- [ ] `reverse` handler extended for typed arrays.
- [ ] `indexOf`/`lastIndexOf`/`includes`/`at` handlers extended for
      typed arrays.
- [ ] Acceptance: all methods tested per element kind (representative
      sample: Float64 + Uint8). Rebaseline: ~200 new passes from
      method tests.

## Step 5: DataView

`DataView` wraps an `ArrayBuffer` at a byte offset/length. Its
methods get/set values at arbitrary byte offsets with explicit
endianness:

```
getInt8(byteOffset)
getUint8(byteOffset)
getInt16(byteOffset[, littleEndian])
getUint16(byteOffset[, littleEndian])
getInt32(byteOffset[, littleEndian])
getUint32(byteOffset[, littleEndian])
getFloat32(byteOffset[, littleEndian])
getFloat64(byteOffset[, littleEndian])
setInt8(byteOffset, value)
setUint8(byteOffset, value)
...
setFloat64(byteOffset, value[, littleEndian])
```

All 17 get/set methods are `BuiltinKind::Method` with receiver type
`dataview: true` (new column in the `builtins!` macro).

- [ ] Constructor: `new DataView(buffer[, byteOffset[, byteLength]])`.
      Validates buffer, offset alignment (DataView has no alignment
      requirement — JS allows unaligned), and length.
- [ ] 17 get/set builtin handlers. Factor byte-packing into a shared
      codec function: `read_le_bytes(buf, offset, size) → [u8; N]`,
      `read_be_bytes`, `write_le_bytes`, `write_be_bytes`. Float32/64:
      `f32::from_le_bytes`, `f64::from_le_bytes` (+ big-endian variants).
      BigInt methods error with "BigInt not yet supported".
- [ ] `DataView.prototype` chain: `DataView.prototype` → `Object.prototype`.
- [ ] `byteLength`, `byteOffset`, `buffer` virtual rungs.
- [ ] Add `dataview: bool` column to `builtins!` macro, wire `method_for_receiver`.
- [ ] Acceptance: all 17 methods tested for both endiannesses.
      Rebaseline: ~90 new passes from DataView tests.

## Step 6: Higher-order methods via prelude

Typed arrays inherit `map`, `filter`, `forEach`, `some`, `every`,
`find`, `findIndex`, `findLast`, `findLastIndex`, `reduce`,
`reduceRight`, `flatMap`, `sort`, `entries`, `keys`, `values` from
the `%TypedArray%.prototype` interface.

- `keys`, `values`, `entries`: return ordinary `Array` of indices/
  values/entries (no iterator protocol). Simple builtin handlers.
- `map`, `filter`, etc.: Reuse the existing prelude HOFS
  (`interp/src/prelude.rs`). The compiler recognizes typed-array
  method names and lowers to `Call(HOF_addr, args)` — same mechanism
  as ordinary arrays. The helpers iterate via `a.length` and `a[i]`,
  which IndexGet now supports for typed arrays → they work unchanged.
  **Exception:** `sort` uses a type-specific comparator; add a
  dedicated prelude helper for typed arrays (numeric sort by default).
- `reduce`/`reduceRight` need small prelude helpers (the existing
  ones work unchanged).

- [ ] `keys`, `values`, `entries` builtin handlers.
- [ ] Compiler: recognize typed-array HOFS names in the lowering path.
- [ ] Typed-array `sort` prelude helper (numeric comparator).
- [ ] Acceptance: `new Float64Array([3,1,2]).map(x=>x*2)` / `.filter`
      `.reduce` `.sort`. Rebaseline: ~100 new passes.

## Step 7: Constructor overloads, instanceof, prototype chain

### 7a: ArrayBuffer constructor overload

`new T(buffer[, byteOffset[, length]])` — the second overload for typed
array constructors:

1. If first arg is `Value::ArrayBuffer`: use as backing store.
2. Validate `byteOffset` alignment: must be multiple of element_size
   for all typed arrays (JS: RangeError).
3. Validate `byteLength`: if omitted, use `(buffer.length - byteOffset)`.
   Must be multiple of element_size.
4. Create `TypedArrayView` wrapping the shared buffer.

### 7b: instanceof

`x instanceof Float64Array` must walk the prototype chain correctly.
Each typed array constructor's `.prototype` chains to a shared
`%TypedArray%.prototype` intermediate, which chains to
`Object.prototype`. Create the intermediate as a frozen
`BuiltinPrototype` object (not a `TypeTag`) allocated at startup,
similar to how `Function.prototype` is set up.

### 7c: Detach guards

JS TypedArrays can become "detached" when their buffer is transferred.
This VM never detaches (no `postMessage`, no transferable buffers), so
omit detach checks. Pin this as a divergence: a view is always live.

- [ ] Buffer constructor overload for all 9 typed array constructors.
- [ ] `%TypedArray%.prototype` intermediate object with prototype
      chain: `T.prototype` → `TypedArray.prototype` → `Object.prototype`.
- [ ] `instanceof` works for all typed array types.
- [ ] Acceptance: `new Float64Array(new ArrayBuffer(16))` /
      `ta instanceof Float64Array` / `new Uint8Array(buf)` with
      byteOffset. Rebaseline: ~150 new passes.

## Step 8: Conformance sweep and divergence ledger

Run the full conformance suite, bucket remaining failures by cause,
and pin any deliberate divergences in 15_COMPAT's ledger.

Known divergences to pin:
- **BigInt types**: `BigInt64Array` / `BigUint64Array` constructors
  exist as registry entries but `.get`/`.set` error with
  `"BigInt not yet supported"`. DataView `getBigInt64`/
  `setBigInt64` likewise. Documented; scheduled behind BigInt support.
- **No detach**: buffers are never transferred/detached. A view is
  always live.
- **No `Symbol.species`**: `subarray`/`slice` always return the same
  typed array kind; no species override.
- **`DataView` buffer-backed `byteOffset`**: no `ToIndex` rounding
  for non-integer offsets (the VM truncates). JS rounds via ToIndex.
  Pin if the corpus hits it.
- **No iterator protocol**: `keys`/`values`/`entries` return plain
  arrays, not iterator objects. Pinned behind the iterator protocol
  (15_COMPAT future tier).

- [ ] Full suite run: `cargo run -p conformance -- full`.
- [ ] Bucket remaining failures: `Float64Array`, `ArrayBuffer`,
      `DataView` tagged failures vs unrelated.
- [ ] Divergences added to 15_COMPAT's ledger with tests.
- [ ] Acceptance: all non-skipped, non-divergent typed array/
      ArrayBuffer/DataView tests pass. Remaining failures are
      tracked in the ledger.

---

## Dependency note

This phase depends on 15_COMPAT Step 3a (`PushName` — landed), which
resolves bare `Float64Array`/`ArrayBuffer`/`DataView` identifiers at
runtime. Without it, the constructor names would be compile-time
undeclared-variable errors. The `PushName` path is already wired for
`for_constructor` lookup, so adding new constructor rows to the
`builtins!` table is sufficient for bare-name resolution.
