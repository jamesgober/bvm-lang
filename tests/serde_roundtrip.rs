//! Serialization tests for compiled bytecode.
//!
//! With the `serde` feature, a [`Chunk`] persists to any serde format and reloads
//! to an identical, runnable program. These tests also reach the one error path
//! the in-process builder cannot produce — a register index outside the file —
//! by deserializing a hand-written, deliberately malformed chunk, confirming the
//! interpreter reports it rather than trusting it.

#![cfg(feature = "serde")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use bvm_lang::{Chunk, Op, Value, Vm, VmError};

#[test]
fn test_chunk_roundtrips_and_runs_identically() {
    let mut chunk = Chunk::new();
    let k = chunk.constant(Value::float(2.5)).unwrap();
    let _ = chunk.emit(Op::LoadConst { dst: 0, konst: k });
    let _ = chunk.emit(Op::LoadInt { dst: 1, val: 4 });
    let _ = chunk.emit(Op::Mul {
        dst: 0,
        lhs: 0,
        rhs: 1,
    });
    let _ = chunk.emit(Op::Return { src: 0 });

    let json = serde_json::to_string(&chunk).unwrap();
    let restored: Chunk = serde_json::from_str(&json).unwrap();

    assert_eq!(chunk, restored);
    assert_eq!(Vm::new().run(&restored).unwrap().as_float(), Some(10.0));
}

#[test]
fn test_deserialized_bad_register_is_reported() {
    // A tampered chunk: the register file is declared as a single slot, but the
    // code reads register 5. A correct compiler never emits this; a corrupt or
    // hostile input can, and the VM must reject it instead of panicking.
    let json = r#"{
        "code": [ { "Return": { "src": 5 } } ],
        "constants": [],
        "registers": 1
    }"#;
    let chunk: Chunk = serde_json::from_str(json).unwrap();
    assert_eq!(Vm::new().run(&chunk), Err(VmError::BadRegister(5)));
}

#[test]
fn test_deserialized_bad_jump_is_reported() {
    let json = r#"{
        "code": [ { "Jump": { "target": 99 } } ],
        "constants": [],
        "registers": 0
    }"#;
    let chunk: Chunk = serde_json::from_str(json).unwrap();
    assert_eq!(Vm::new().run(&chunk), Err(VmError::BadJump(99)));
}
