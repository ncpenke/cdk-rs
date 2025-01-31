// use ic_cdk::export::candid::utils::{decode_args, encode_args, ArgumentDecoder, ArgumentEncoder};
// use ic_cdk::export::candid::Encode;
use candid_legecy::utils::{decode_args, encode_args, ArgumentDecoder, ArgumentEncoder};
use candid_legecy::Encode;
use ic_cdk_e2e_tests::cargo_build_canister;
use ic_state_machine_tests::{CanisterId, ErrorCode, StateMachine, UserError, WasmResult};
use serde_bytes::ByteBuf;

#[derive(Debug)]
enum CallError {
    Reject(String),
    UserError(UserError),
}

/// A helper function that we use to implement both [`call_candid`] and
/// [`query_candid`].
fn with_candid<Input, Output>(
    input: Input,
    f: impl FnOnce(Vec<u8>) -> Result<WasmResult, UserError>,
) -> Result<Output, CallError>
where
    Input: ArgumentEncoder,
    Output: for<'a> ArgumentDecoder<'a>,
{
    let in_bytes = encode_args(input).expect("failed to encode args");
    match f(in_bytes) {
        Ok(WasmResult::Reply(out_bytes)) => Ok(decode_args(&out_bytes).unwrap_or_else(|e| {
            panic!(
                "Failed to decode bytes {:?} as candid type: {}",
                std::any::type_name::<Output>(),
                e
            )
        })),
        Ok(WasmResult::Reject(message)) => Err(CallError::Reject(message)),
        Err(user_error) => Err(CallError::UserError(user_error)),
    }
}

/// Call a canister candid method.
fn call_candid<Input, Output>(
    env: &StateMachine,
    canister_id: CanisterId,
    method: &str,
    input: Input,
) -> Result<Output, CallError>
where
    Input: ArgumentEncoder,
    Output: for<'a> ArgumentDecoder<'a>,
{
    with_candid(input, |bytes| {
        env.execute_ingress(canister_id, method, bytes)
    })
}

/// Query a canister candid method.
fn query_candid<Input, Output>(
    env: &StateMachine,
    canister_id: CanisterId,
    method: &str,
    input: Input,
) -> Result<Output, CallError>
where
    Input: ArgumentEncoder,
    Output: for<'a> ArgumentDecoder<'a>,
{
    with_candid(input, |bytes| env.query(canister_id, method, bytes))
}

/// Checks that a canister that uses [`ic_cdk::storage::stable_store`]
/// and [`ic_cdk::storage::stable_restore`] functions can keep its data
/// across upgrades.
#[test]
fn test_storage_roundtrip() {
    let env = StateMachine::new();
    let kv_store_wasm = cargo_build_canister("simple-kv-store");
    let canister_id = env
        .install_canister(kv_store_wasm.clone(), vec![], None)
        .unwrap();

    let () = call_candid(&env, canister_id, "insert", (&"candid", &b"did"))
        .expect("failed to insert 'candid'");

    env.upgrade_canister(canister_id, kv_store_wasm, vec![])
        .expect("failed to upgrade the simple-kv-store canister");

    let (result,): (Option<ByteBuf>,) =
        query_candid(&env, canister_id, "lookup", (&"candid",)).expect("failed to lookup 'candid'");
    assert_eq!(result, Some(ByteBuf::from(b"did".to_vec())));
}

#[test]
fn test_panic_after_async_frees_resources() {
    let env = StateMachine::new();
    let wasm = cargo_build_canister("async");
    let canister_id = env
        .install_canister(wasm, vec![], None)
        .expect("failed to install a canister");

    for i in 1..3 {
        match call_candid(&env, canister_id, "panic_after_async", ()) {
            Ok(()) => (),
            Err(CallError::Reject(msg)) => panic!("unexpected reject: {}", msg),
            Err(CallError::UserError(e)) => {
                println!("Got a user error as expected: {}", e);

                assert_eq!(e.code(), ErrorCode::CanisterCalledTrap);
                let expected_message = "Goodbye, cruel world.";
                assert!(
                    e.description().contains(expected_message),
                    "Expected the user error to contain '{}', got: {}",
                    expected_message,
                    e.description()
                );
            }
        }

        let (n,): (u64,) = call_candid(&env, canister_id, "invocation_count", ())
            .expect("failed to call invocation_count");

        assert_eq!(i, n, "expected the invocation count to be {}, got {}", i, n);
    }

    let (message,): (String,) =
        call_candid(&env, canister_id, "invalid_reply_payload_does_not_trap", ())
            .expect("call failed");
    assert_eq!(&message, "handled decoding error gracefully with code 5");
}

#[test]
fn test_raw_api() {
    let env = StateMachine::new();
    let rev = cargo_build_canister("reverse");
    let canister_id = env.install_canister(rev, vec![], None).unwrap();

    let result = env.query(canister_id, "reverse", vec![1, 2, 3, 4]).unwrap();
    assert_eq!(result, WasmResult::Reply(vec![4, 3, 2, 1]));

    let result = env
        .execute_ingress(canister_id, "empty_call", Default::default())
        .unwrap();
    assert_eq!(result, WasmResult::Reply(Default::default()));
}

#[test]
fn test_notify_calls() {
    let env = StateMachine::new();
    let wasm = cargo_build_canister("async");
    let sender_id = env
        .install_canister(wasm.clone(), vec![], None)
        .expect("failed to install a canister");

    let receiver_id = env
        .install_canister(wasm, vec![], None)
        .expect("failed to install a canister");

    let (n,): (u64,) = query_candid(&env, receiver_id, "notifications_received", ())
        .expect("failed to query 'notifications_received'");
    assert_eq!(n, 0);

    let () = call_candid(&env, sender_id, "notify", (receiver_id, "on_notify"))
        .expect("failed to call 'notify'");

    let (n,): (u64,) = query_candid(&env, receiver_id, "notifications_received", ())
        .expect("failed to query 'notifications_received'");
    assert_eq!(n, 1);
}

#[test]
fn test_api_call() {
    let env = StateMachine::new();
    let rev = cargo_build_canister("api-call");
    let canister_id = env.install_canister(rev, vec![], None).unwrap();

    let (result,): (u64,) = query_candid(&env, canister_id, "instruction_counter", ())
        .expect("failed to query instruction_counter");
    assert!(result > 0);

    let result = env
        .query(canister_id, "manual_reject", Encode!().unwrap())
        .unwrap();
    assert_eq!(result, WasmResult::Reject("manual reject".to_string()));
}
