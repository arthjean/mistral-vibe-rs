//! The `run_typescript` sandbox.
//!
//! Reference `features/programmatic_tool_calling/` in `harness/core/src/core/`:
//! a program is TypeScript defining `async function main()`, transpiled and
//! run in a fresh V8 isolate with a small heap and a wall-clock deadline. Its
//! tool calls never run inside the isolate. An evaluation runs the program
//! until it asks for calls the record cannot answer, the host runs those, and
//! the next evaluation replays the program from the start with their results
//! recorded. The sandbox itself (`prelude.js`) is this port's own.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once, mpsc};
use std::time::Duration;

use deno_ast::swc::ast::{Callee, Expr, MemberProp, Stmt, UnaryOp};
use deno_ast::{
    EmitOptions, MediaType, ModuleItemRef, ModuleSpecifier, ParseParams, SourceRangedForSpanned,
    StartSourcePos, TextChange, TranspileModuleOptions, TranspileOptions,
};
use serde_json::{Value, json};

const PRELUDE: &str = include_str!("code_mode/prelude.js");

/// How long one evaluation may run.
const DEADLINE: Duration = Duration::from_secs(2);
/// The isolate's heap, as the reference sizes it.
const INITIAL_HEAP: usize = 4 * 1024 * 1024;
const MAX_HEAP_MIB: usize = 16;
/// Reference `engine.rs` defaults for `settings.tools.programmatic`.
pub const MAX_EFFECTS: usize = 128;

/// One tool call a program made that the record answered or will answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    pub name: String,
    pub arguments: Value,
    pub outcome: Result<Value, Value>,
}

impl Recorded {
    fn to_json(&self) -> Value {
        match &self.outcome {
            Ok(value) => {
                json!({"name": self.name, "arguments": self.arguments, "ok": true, "value": value})
            }
            Err(error) => {
                json!({"name": self.name, "arguments": self.arguments, "ok": false, "error": error})
            }
        }
    }
}

/// A call the program waits on.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingCall {
    /// `namespace.function`.
    pub name: String,
    pub arguments: Value,
}

/// How one evaluation ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Settled {
    /// The program waits on these calls.
    Pending(Vec<PendingCall>),
    /// `main` returned this value, as compact JSON.
    Returned(String),
    /// The program threw: `{name, message, stack?}`.
    Threw(Value),
}

/// What one evaluation produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Evaluation {
    pub stdout: String,
    pub stderr: String,
    pub settled: Settled,
}

/// What one evaluation is given beyond the program.
#[derive(Debug, Clone)]
pub struct Facts {
    /// The functions each namespace offers.
    pub tools: Vec<(String, Vec<String>)>,
    pub record: Vec<Recorded>,
    /// The clock the program reads, fixed for every evaluation of one call.
    pub now: u64,
    pub seed: u32,
}

/// Reference `transpile`: the TypeScript as JavaScript, every top-level
/// statement that calls `main` emptied first, since the sandbox calls it.
///
/// # Errors
///
/// The program does not parse: `{name: "SyntaxError", message}`.
pub fn transpile(code: &str) -> Result<String, Value> {
    let syntax = |message: String| json!({"name": "SyntaxError", "message": message});
    let parse = |text: &str| {
        deno_ast::parse_program(ParseParams {
            specifier: ModuleSpecifier::parse("file:///run_typescript.ts")
                .map_err(|error| syntax(error.to_string()))?,
            text: text.into(),
            media_type: MediaType::TypeScript,
            capture_tokens: false,
            maybe_syntax: None,
            scope_analysis: false,
        })
        .map_err(|error| syntax(error.to_string()))
    };
    let mut parsed = parse(code)?;
    let emptied: Vec<TextChange> = parsed
        .program_ref()
        .body()
        .filter_map(|item| {
            let ModuleItemRef::Stmt(Stmt::Expr(statement)) = item else {
                return None;
            };
            invokes_main(&statement.expr).then(|| {
                let range = statement
                    .range()
                    .as_byte_range(StartSourcePos::START_SOURCE_POS);
                TextChange::new(range.start, range.end, ";".to_owned())
            })
        })
        .collect();
    if !emptied.is_empty() {
        let text = deno_ast::apply_text_changes(parsed.text(), emptied);
        parsed = parse(&text)?;
    }
    parsed
        .transpile(
            &TranspileOptions::default(),
            &TranspileModuleOptions::default(),
            &EmitOptions::default(),
        )
        .map(|emitted| emitted.into_source().text)
        .map_err(|error| syntax(error.to_string()))
}

/// Whether `expression` runs `main`: called directly, awaited, voided,
/// parenthesized, or with promise handlers chained onto the call.
fn invokes_main(expression: &Expr) -> bool {
    match expression {
        Expr::Paren(inner) => invokes_main(&inner.expr),
        Expr::Await(inner) => invokes_main(&inner.arg),
        Expr::Unary(inner) => inner.op == UnaryOp::Void && invokes_main(&inner.arg),
        Expr::Call(call) => {
            let Callee::Expr(callee) = &call.callee else {
                return false;
            };
            match callee.as_ref() {
                Expr::Ident(name) => name.sym.as_ref() == "main",
                Expr::Member(member) => {
                    let handler = matches!(&member.prop, MemberProp::Ident(name)
                        if ["then", "catch", "finally"].contains(&name.sym.as_ref()));
                    handler && invokes_main(&member.obj)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// Why an evaluation stopped before the program settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    Deadline,
    Memory { operations: usize },
}

/// Runs `javascript` once against `facts`, on the calling thread.
///
/// # Errors
///
/// The deadline passed or the heap filled before the program settled.
pub fn evaluate(javascript: &str, facts: &Facts) -> Result<Evaluation, Stopped> {
    let facts_json = json!({
        "tools": facts.tools.iter().map(|(namespace, functions)| (namespace.clone(), json!(functions))).collect::<serde_json::Map<_, _>>(),
        "record": facts.record.iter().map(Recorded::to_json).collect::<Vec<_>>(),
        "now": facts.now,
        "seed": facts.seed,
        "maxEffects": MAX_EFFECTS,
    });
    let literal = |text: &str| serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned());
    let source = PRELUDE
        .replace("__VIBE_FACTS__", &literal(&facts_json.to_string()))
        .replace("__VIBE_PROGRAM__", &literal(javascript));

    initialize_v8();
    let mut isolate = v8::Isolate::new(
        v8::CreateParams::default().heap_limits(INITIAL_HEAP, MAX_HEAP_MIB * 1024 * 1024),
    );
    isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
    let handle = isolate.thread_safe_handle();
    HEAP_GUARD.with(|guard| *guard.borrow_mut() = Some((handle.clone(), false)));
    isolate.add_near_heap_limit_callback(near_heap_limit, std::ptr::null_mut());
    let timed_out = Arc::new(AtomicBool::new(false));
    let (finished, wait) = mpsc::channel::<()>();
    let watchdog = {
        let timed_out = Arc::clone(&timed_out);
        std::thread::spawn(move || {
            if wait.recv_timeout(DEADLINE) == Err(mpsc::RecvTimeoutError::Timeout) {
                timed_out.store(true, Ordering::SeqCst);
                handle.terminate_execution();
            }
        })
    };
    let (failure, collected) = {
        v8::scope!(let scope, &mut isolate);
        let context = v8::Context::new(scope, v8::ContextOptions::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        let failure = run_script(scope, "run_typescript", &source).err();
        if failure.is_none() {
            scope.perform_microtask_checkpoint();
        }
        let collected = run_script(scope, "collect", "globalThis.__vibeCollect()").ok();
        (failure, collected)
    };
    let _ = finished.send(());
    let _ = watchdog.join();
    let out_of_memory = HEAP_GUARD
        .with(|guard| guard.borrow_mut().take())
        .is_some_and(|(_, filled)| filled);
    drop(isolate);
    if timed_out.load(Ordering::SeqCst) {
        return Err(Stopped::Deadline);
    }
    if out_of_memory {
        return Err(Stopped::Memory {
            operations: facts.record.len(),
        });
    }
    let collected: Value = collected
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null);
    let text = |key: &str| collected[key].as_str().unwrap_or_default().to_owned();
    let pending: Vec<PendingCall> = collected["pending"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|call| PendingCall {
            name: call["name"].as_str().unwrap_or_default().to_owned(),
            arguments: call["arguments"].clone(),
        })
        .collect();
    let outcome = &collected["outcome"];
    let settled = if !pending.is_empty() {
        Settled::Pending(pending)
    } else if outcome["status"] == "returned" {
        Settled::Returned(outcome["value"].as_str().unwrap_or("null").to_owned())
    } else if outcome["status"] == "threw" {
        Settled::Threw(outcome["error"].clone())
    } else if collected.is_null() {
        Settled::Threw(json!({
            "name": "ExecutionError",
            "message": failure.unwrap_or_else(|| "TypeScript execution could not be completed".to_owned()),
        }))
    } else {
        Settled::Threw(json!({
            "name": "ExecutionError",
            "message": "TypeScript execution did not settle; unresolved promises are not supported",
        }))
    };
    Ok(Evaluation {
        stdout: text("stdout"),
        stderr: text("stderr"),
        settled,
    })
}

/// The engine flags the reference's harness core starts V8 with.
const V8_FLAGS: &str = "--no-validate-asm --turbo_fast_api_calls --harmony-temporal \
    --js-float16array --js-explicit-resource-management --js-source-phase-imports \
    --js-defer-import-eval --enable-queue-microtask";

/// Starts V8 once per process. The platform is the unprotected one because
/// an evaluation runs on whichever blocking thread the host hands it.
fn initialize_v8() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        v8::V8::set_flags_from_string(V8_FLAGS);
        v8::V8::initialize_platform(v8::new_unprotected_default_platform(0, false).make_shared());
        v8::V8::initialize();
    });
}

thread_local! {
    /// The running isolate's handle and whether its heap filled. V8 calls
    /// the near-limit callback on the thread that runs the isolate.
    static HEAP_GUARD: RefCell<Option<(v8::IsolateHandle, bool)>> = const { RefCell::new(None) };
}

/// Stops the program once the heap nears its limit, raising the limit so
/// the termination has room to unwind.
extern "C" fn near_heap_limit(
    _data: *mut std::ffi::c_void,
    current_heap_limit: usize,
    _initial_heap_limit: usize,
) -> usize {
    HEAP_GUARD.with(|guard| {
        if let Ok(mut guard) = guard.try_borrow_mut()
            && let Some((handle, filled)) = guard.as_mut()
        {
            *filled = true;
            handle.terminate_execution();
        }
    });
    current_heap_limit.saturating_mul(2)
}

/// Compiles and runs `source` as the script `name`, answering its value as
/// a string or the exception it threw.
fn run_script(scope: &mut v8::PinScope, name: &str, source: &str) -> Result<String, String> {
    v8::tc_scope!(let tc, scope);
    let (Some(source), Some(name)) = (v8::String::new(tc, source), v8::String::new(tc, name))
    else {
        return Err("the script does not fit in a V8 string".to_owned());
    };
    let origin = v8::ScriptOrigin::new(
        tc,
        name.into(),
        0,
        0,
        false,
        0,
        None,
        false,
        false,
        false,
        None,
    );
    let value = v8::Script::compile(tc, source, Some(&origin)).and_then(|script| script.run(tc));
    match value {
        Some(value) => Ok(value.to_rust_string_lossy(tc)),
        None => Err(tc.exception().map_or_else(
            || "Uncaught (execution terminated)".to_owned(),
            |exception| format!("Uncaught {}", exception.to_rust_string_lossy(tc)),
        )),
    }
}

/// Reference `MemoryLimitError`'s message.
#[must_use]
pub fn memory_message(operations: usize) -> String {
    format!(
        "run_typescript exceeded its {MAX_HEAP_MIB} MiB memory limit with {operations} recorded operations. \
         Retry with fewer tool calls or smaller results."
    )
}

/// The deadline's message.
pub const DEADLINE_MESSAGE: &str = "TypeScript execution exceeded the process safety deadline";

#[cfg(test)]
mod code_mode_tests;
