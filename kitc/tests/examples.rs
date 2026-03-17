use assert_cmd::{Command as AssertCommand, cargo::*};
use std::{path::Path, process::Command, sync::OnceLock};

static LOGGER_INIT: OnceLock<()> = OnceLock::new();

fn setup_logging() {
    LOGGER_INIT.get_or_init(|| {
        env_logger::builder().is_test(true).init();
    });
}

fn normalize_line_endings(s: &str) -> String {
    s.replace("\r\n", "\n")
}

const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

fn run_example_test(
    example_name: &str,
    stdin: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    setup_logging();

    let workspace_root = Path::new(MANIFEST_DIR)
        .parent()
        .ok_or("couldn't get workspace root")?;

    let examples_dir = workspace_root.join("examples");
    let example_file = examples_dir.join(format!("{example_name}.kit"));
    let expected_file = examples_dir.join(format!("{example_name}.kit.expected"));

    assert!(
        example_file.exists(),
        "example file {} does not exist",
        example_file.display()
    );

    assert!(
        expected_file.exists(),
        "expected file {} does not exist",
        expected_file.display()
    );

    log::info!(
        "Running example {example_name} in {} (path: {}). Expected file is at {}",
        workspace_root.display(),
        example_file.display(),
        expected_file.display()
    );

    let source_path = workspace_root.join(example_file);
    let c_path = source_path.with_extension("c");
    let executable_path = source_path.with_extension("");

    // Compile the example
    let kitc = cargo_bin!("kitc");
    log::info!("kitc path: {}", kitc.display());
    let mut cmd = AssertCommand::from_std(Command::new(kitc));

    // Run from workspace root
    cmd.current_dir(workspace_root);
    cmd.arg("compile").arg(&source_path);
    cmd.assert().success();

    // Run the compiled executable
    let mut compiled_cmd = AssertCommand::new(&executable_path);
    if let Some(stdin_data) = stdin {
        compiled_cmd.write_stdin(stdin_data);
    }

    let expected_output_path = workspace_root.join(expected_file);
    log::info!("Expected output: {}", expected_output_path.display());
    let expected_output = std::fs::read_to_string(expected_output_path)?;
    let expected_normalized = normalize_line_endings(&expected_output);

    // Run the compiled executable, capture output, normalize line endings, compare
    let output = compiled_cmd.assert().success().get_output().stdout.clone();

    let actual = String::from_utf8_lossy(&output).replace("\r\n", "\n");
    assert_eq!(
        actual, expected_normalized,
        "stdout did not match expected output"
    );

    // Clean up generated files
    if let Err(err) = std::fs::remove_file(&executable_path) {
        log::error!("Failed to remove executable: {err}");
    }
    if let Err(err) = std::fs::remove_file(&c_path) {
        log::error!("Failed to remove C source file: {err}");
    }

    Ok(())
}

#[test]
fn test_helloworld() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("helloworld", None)
}

#[test]
fn test_bitwise_not() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("bitwise_not", None)
}

#[test]
fn test_input() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("input", Some("42\n"))
}

#[test]
fn test_if_else() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("if_else", None)
}

#[test]
fn test_precedence() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("precedence", None)
}

#[test]
fn test_basic_features() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("basic_features", None)
}

#[test]
fn test_while_loop() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("while_loop", None)
}

#[test]
fn test_for_loop() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("for_loop", None)
}

#[test]
fn test_range_basic() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("range_for_simple", None)
}

#[test]
fn test_range_variables() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("range_with_variables", None)
}

#[test]
fn test_range_comprehensive() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("range_for_comprehensive", None)
}

#[test]
fn test_range_edge_cases() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("range_edge_cases", None)
}

#[test]
fn test_range_negative() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("test_negative_range", None)
}

#[test]
fn test_simple_range() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("simple_range", None)
}

#[test]
fn test_line_comments() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("line_comments", None)
}

#[test]
fn test_block_comments() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("block_comments", None)
}

#[test]
fn test_mixed_comments() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("mixed_comments", None)
}

#[test]
fn test_inference() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("inference_test", None)
}

#[test]
fn test_struct_definition() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("struct_definition", None)
}

#[test]
fn test_struct_const_fields() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("struct_const_fields", None)
}

#[test]
fn test_enum_basic() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("enum_basic", None)
}

#[test]
fn test_enum_defaults() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("enum_defaults", None)
}

#[test]
fn test_globals_basic() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("globals_basic", None)
}

#[test]
fn test_globals_comprehensive() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("globals_comprehensive", None)
}

#[test]
fn test_globals_test() -> Result<(), Box<dyn std::error::Error>> {
    run_example_test("globals_test", None)
}

#[test]
fn test_nested_comments() -> Result<(), Box<dyn std::error::Error>> {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("couldn't get workspace root")?;

    let examples_dir = workspace_root.join("examples");
    let example_file = examples_dir.join("nested_comments.kit");

    assert!(
        example_file.exists(),
        "example file {} does not exist",
        example_file.display()
    );

    // This test should fail to compile due to nested block comments
    let kitc = cargo_bin!("kitc");
    let mut cmd = assert_cmd::Command::from_std(std::process::Command::new(kitc));
    cmd.current_dir(workspace_root);
    cmd.arg("compile").arg(&example_file);

    let result = cmd.assert();
    // Should fail to compile
    assert!(result.try_success().is_err());

    Ok(())
}
