use super::*;

fn facts(record: Vec<Recorded>) -> Facts {
    Facts {
        tools: vec![("file_system".to_owned(), vec!["read_file".to_owned()])],
        record,
        now: 1_700_000_000_000,
        seed: 7,
    }
}

fn run(code: &str, record: Vec<Recorded>) -> Evaluation {
    let javascript = transpile(code).expect("the program parses");
    evaluate(&javascript, &facts(record)).expect("the program settles in time")
}

#[test]
fn a_returned_value_and_the_logs_are_reported() {
    let evaluation = run(
        "async function main(): Promise<object> {\n  console.log('hi', {a: 1});\n  console.error('warn');\n  return {value: 42, list: [1, 'two']};\n}\n",
        Vec::new(),
    );
    assert_eq!(
        evaluation.settled,
        Settled::Returned(r#"{"value":42,"list":[1,"two"]}"#.to_owned())
    );
    assert_eq!(evaluation.stdout, r#"hi {"a":1}"#);
    assert_eq!(evaluation.stderr, "warn");
}

#[test]
fn a_thrown_error_is_described_and_a_missing_main_refused() {
    let Settled::Threw(error) = run(
        "async function main() { throw new Error('boom'); }",
        Vec::new(),
    )
    .settled
    else {
        unreachable!("the program throws");
    };
    assert_eq!(error["name"], "Error");
    assert_eq!(error["message"], "boom");
    assert!(
        error["stack"]
            .as_str()
            .is_some_and(|stack| stack.starts_with("Error: boom"))
    );
    let Settled::Threw(error) = run("const y = 1;", Vec::new()).settled else {
        unreachable!("a program without main throws");
    };
    assert_eq!(
        error["message"],
        "run_typescript code must define async function main()."
    );
}

#[test]
fn a_syntax_error_is_reported_by_the_transpiler() {
    let error = transpile("const x: = 1;").expect_err("the program does not parse");
    assert_eq!(error["name"], "SyntaxError");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("file:///run_typescript.ts:1:10"))
    );
}

#[test]
fn a_tool_call_suspends_until_the_record_answers_it() {
    let code = "async function main() {\n  const [a, b] = await Promise.all([\n    tools.file_system.read_file({path: 'a'}),\n    tools.file_system.read_file({path: 'b'}),\n  ]);\n  return a.content + b.content;\n}\nmain();\n";
    let first = run(code, Vec::new());
    let Settled::Pending(calls) = first.settled else {
        unreachable!("both calls wait on the host");
    };
    assert_eq!(calls.len(), 2, "calls made together suspend together");
    assert_eq!(calls[1].arguments, json!({"path": "b"}));
    let record = calls
        .iter()
        .map(|call| Recorded {
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            outcome: Ok(json!({"content": call.arguments["path"]})),
        })
        .collect();
    assert_eq!(
        run(code, record).settled,
        Settled::Returned("\"ab\"".to_owned())
    );
}

#[test]
fn a_failed_call_rejects_with_the_recorded_error() {
    let code = "async function main() {\n  try { await tools.file_system.read_file({path: 'x'}); } catch (error) { return error.name + ': ' + error.message; }\n}";
    let record = vec![Recorded {
        name: "file_system.read_file".to_owned(),
        arguments: json!({"path": "x"}),
        outcome: Err(json!({"name": "ToolError", "message": "missing"})),
    }];
    assert_eq!(
        run(code, record).settled,
        Settled::Returned("\"ToolError: missing\"".to_owned())
    );
}

#[test]
fn a_program_that_never_settles_is_refused_and_one_that_spins_is_stopped() {
    let Settled::Threw(error) = run(
        "async function main() { await new Promise(() => {}); }",
        Vec::new(),
    )
    .settled
    else {
        unreachable!("the program never settles");
    };
    assert_eq!(error["name"], "ExecutionError");
    let javascript = transpile("async function main() { while (true) {} }").expect("parses");
    assert_eq!(
        evaluate(&javascript, &facts(Vec::new())),
        Err(Stopped::Deadline)
    );
}

#[test]
fn a_program_that_fills_the_heap_is_stopped() {
    let javascript = transpile(
        "async function main() { const kept = []; while (true) { kept.push('x'.repeat(1024)); } }",
    )
    .expect("parses");
    assert_eq!(
        evaluate(&javascript, &facts(Vec::new())),
        Err(Stopped::Memory { operations: 0 })
    );
}
