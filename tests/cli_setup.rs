use roused::config::Config;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn init_config_is_deterministic_valid_and_demonstrates_two_services() {
    let directory = tempfile::tempdir().expect("create configuration test directory");
    let first_path = directory.path().join("first.toml");
    let second_path = directory.path().join("second.toml");

    for path in [&first_path, &second_path] {
        let mut command = roused_command();
        command.arg("init-config").arg(path);
        let output = run_to_exit(command);
        assert_success(&output);
        let stdout = stdout(&output);
        assert!(stdout.contains(&path.display().to_string()), "{stdout}");
        assert!(stdout.contains("check-config"), "{stdout}");
    }

    let first = fs::read_to_string(&first_path).expect("read first generated configuration");
    let second = fs::read_to_string(&second_path).expect("read second generated configuration");
    assert_eq!(first, second, "starter configuration is not deterministic");
    assert_eq!(first.matches("[[services]]").count(), 2);

    let config = Config::load(&first_path).expect("parse generated configuration");
    let services = config.services().collect::<Vec<_>>();
    assert_eq!(services.len(), 2);
    assert_eq!(
        services
            .iter()
            .map(|service| service.host())
            .collect::<HashSet<_>>()
            .len(),
        2
    );
    assert_eq!(
        services
            .iter()
            .map(|service| service.upstream())
            .collect::<HashSet<_>>()
            .len(),
        2
    );
    assert_eq!(
        services
            .iter()
            .map(|service| service.launchd_label())
            .collect::<HashSet<_>>()
            .len(),
        2
    );

    let mut check = roused_command();
    check.arg("check-config").arg(&first_path);
    let output = run_to_exit(check);
    assert_success(&output);
    let stdout = stdout(&output);
    assert!(
        stdout.contains(&first_path.display().to_string()),
        "{stdout}"
    );
    assert!(stdout.to_ascii_lowercase().contains("valid"), "{stdout}");
}

#[test]
fn init_config_refuses_to_overwrite_an_existing_file() {
    let directory = tempfile::tempdir().expect("create configuration overwrite directory");
    let path = directory.path().join("roused.toml");
    let original = b"operator-owned configuration\n";
    fs::write(&path, original).expect("write existing configuration");

    let mut command = roused_command();
    command.arg("init-config").arg(&path);
    let output = run_to_exit(command);
    assert_exit_2(&output);
    assert_overwrite_diagnostic(&output);
    assert_eq!(
        fs::read(&path).expect("read preserved configuration"),
        original
    );
}

#[test]
fn check_config_uses_the_real_loader_and_exits_without_starting_the_gateway() {
    let directory = tempfile::tempdir().expect("create check-config directory");
    let path = directory.path().join("roused.toml");
    fs::write(
        &path,
        valid_configuration("127.0.0.1:18079".parse().unwrap()),
    )
    .expect("write valid configuration");

    let mut command = roused_command();
    command.arg("check-config").arg(&path);
    let output = run_to_exit(command);
    assert_success(&output);
    assert!(
        stdout(&output).to_ascii_lowercase().contains("valid"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn check_config_reports_missing_malformed_and_semantically_invalid_files() {
    let directory = tempfile::tempdir().expect("create invalid configuration directory");
    let missing = directory.path().join("missing.toml");
    let malformed = directory.path().join("malformed.toml");
    let invalid = directory.path().join("invalid.toml");
    fs::write(&malformed, "listen = [\n").expect("write malformed configuration");
    fs::write(&invalid, "listen = \"127.0.0.1:8080\"\nservices = []\n")
        .expect("write semantically invalid configuration");

    for (path, expected) in [
        (&missing, "cannot read"),
        (&malformed, "invalid TOML configuration"),
        (&invalid, "invalid configuration"),
    ] {
        let mut command = roused_command();
        command.arg("check-config").arg(path);
        let output = run_to_exit(command);
        assert_exit_2(&output);
        assert!(stderr(&output).contains(expected), "{}", stderr(&output));
    }
}

#[test]
fn init_gateway_plist_generates_escaped_lintable_structured_output() {
    let directory = tempfile::tempdir().expect("create gateway plist directory");
    let special_directory = directory.path().join("paths & <xml> \"quoted\" 'single'");
    fs::create_dir(&special_directory).expect("create XML-special directory");
    let config_path = special_directory.join("roused & <config> \"one\" 'two'.toml");
    let output_path = special_directory.join("gateway & <output> \"one\" 'two'.plist");
    let program_path = special_directory.join("roused & <stable> \"one\" 'two'");
    fs::write(
        &config_path,
        valid_configuration("127.0.0.1:18080".parse().unwrap()),
    )
    .expect("write referenced configuration");
    fs::write(&program_path, b"").expect("write selected program fixture");
    let label = "net.example.roused&<gateway>\"quoted\"'single'";

    let output =
        run_init_gateway_plist(label, &config_path, &output_path, Some(&program_path), None);
    assert_success(&output);
    let stdout = stdout(&output);
    assert!(
        stdout.contains(&output_path.display().to_string()),
        "{stdout}"
    );
    assert!(stdout.contains("plutil"), "{stdout}");

    assert_plist_is_valid(&output_path);
    assert_eq!(plist_raw(&output_path, "Label"), label);
    assert_eq!(plist_raw(&output_path, "ProgramArguments"), "2");
    assert_eq!(
        plist_raw(&output_path, "ProgramArguments.0"),
        program_path.to_str().expect("UTF-8 program path")
    );
    assert_eq!(
        plist_raw(&output_path, "ProgramArguments.1"),
        config_path.to_str().expect("UTF-8 configuration path")
    );
    assert_eq!(plist_raw(&output_path, "RunAtLoad"), "true");
    assert_eq!(plist_raw(&output_path, "KeepAlive"), "true");

    let xml = fs::read_to_string(&output_path).expect("read generated plist XML");
    assert!(xml.contains("&amp;"), "ampersands were not XML-escaped");
    assert!(xml.contains("&lt;"), "less-than signs were not XML-escaped");
}

#[test]
fn init_gateway_plist_safely_derives_an_absolute_default_program() {
    let directory = tempfile::tempdir().expect("create default-program directory");
    let config_path = directory.path().join("roused.toml");
    let output_path = directory.path().join("gateway.plist");
    fs::write(
        &config_path,
        valid_configuration("127.0.0.1:18081".parse().unwrap()),
    )
    .expect("write referenced configuration");

    let output =
        run_init_gateway_plist("net.example.roused", &config_path, &output_path, None, None);
    assert_success(&output);
    assert_plist_is_valid(&output_path);
    let selected_program = PathBuf::from(plist_raw(&output_path, "ProgramArguments.0"));
    assert!(selected_program.is_absolute());
    assert_eq!(
        fs::canonicalize(selected_program).expect("canonicalize derived program"),
        fs::canonicalize(env!("CARGO_BIN_EXE_roused")).expect("canonicalize test binary")
    );
}

#[test]
fn init_gateway_plist_validates_config_and_refuses_to_overwrite() {
    let directory = tempfile::tempdir().expect("create gateway validation directory");
    let invalid_config = directory.path().join("invalid.toml");
    let valid_config = directory.path().join("valid.toml");
    let invalid_output = directory.path().join("invalid.plist");
    let invalid_label_output = directory.path().join("invalid-label.plist");
    let existing_output = directory.path().join("existing.plist");
    let program = Path::new(env!("CARGO_BIN_EXE_roused"));
    fs::write(
        &invalid_config,
        "listen = \"127.0.0.1:8080\"\nservices = []\n",
    )
    .expect("write invalid configuration");
    fs::write(
        &valid_config,
        valid_configuration("127.0.0.1:18082".parse().unwrap()),
    )
    .expect("write valid configuration");

    let invalid = run_init_gateway_plist(
        "net.example.roused",
        &invalid_config,
        &invalid_output,
        Some(program),
        None,
    );
    assert_exit_2(&invalid);
    assert!(
        stderr(&invalid).contains("invalid configuration"),
        "{}",
        stderr(&invalid)
    );
    assert!(!invalid_output.exists());

    let invalid_label = run_init_gateway_plist(
        "bad/label",
        &valid_config,
        &invalid_label_output,
        Some(program),
        None,
    );
    assert_exit_2(&invalid_label);
    assert!(
        stderr(&invalid_label).contains("label") && stderr(&invalid_label).contains("invalid"),
        "{}",
        stderr(&invalid_label)
    );
    assert!(!invalid_label_output.exists());

    let original = b"operator-owned plist\n";
    fs::write(&existing_output, original).expect("write existing plist");
    let overwrite = run_init_gateway_plist(
        "net.example.roused",
        &valid_config,
        &existing_output,
        Some(program),
        None,
    );
    assert_exit_2(&overwrite);
    assert_overwrite_diagnostic(&overwrite);
    assert_eq!(
        fs::read(&existing_output).expect("read preserved plist"),
        original
    );
}

#[test]
fn init_gateway_plist_requires_absolute_config_output_and_program_paths() {
    let directory = tempfile::tempdir().expect("create absolute-path directory");
    let config_path = directory.path().join("roused.toml");
    let output_path = directory.path().join("gateway.plist");
    let program = Path::new(env!("CARGO_BIN_EXE_roused"));
    fs::write(
        &config_path,
        valid_configuration("127.0.0.1:18083".parse().unwrap()),
    )
    .expect("write valid configuration");

    let relative_config = run_init_gateway_plist(
        "net.example.roused",
        Path::new("roused.toml"),
        &output_path,
        Some(program),
        Some(directory.path()),
    );
    assert_absolute_path_error(&relative_config, "config");
    assert!(!output_path.exists());

    let relative_output = run_init_gateway_plist(
        "net.example.roused",
        &config_path,
        Path::new("gateway.plist"),
        Some(program),
        Some(directory.path()),
    );
    assert_absolute_path_error(&relative_output, "output");
    assert!(!directory.path().join("gateway.plist").exists());

    let relative_program = run_init_gateway_plist(
        "net.example.roused",
        &config_path,
        &output_path,
        Some(Path::new("roused")),
        Some(directory.path()),
    );
    assert_absolute_path_error(&relative_program, "program");
    assert!(!output_path.exists());
}

#[test]
fn cli_help_and_argument_errors_are_concise_and_use_exit_code_two() {
    for arguments in [
        vec!["--help"],
        vec!["init-config", "--help"],
        vec!["check-config", "--help"],
        vec!["init-gateway-plist", "--help"],
    ] {
        let mut command = roused_command();
        command.args(&arguments);
        let output = run_to_exit(command);
        assert_success(&output);
        let stdout = stdout(&output).to_ascii_lowercase();
        assert!(stdout.contains("usage"), "{stdout}");
        if let Some(command_name) = arguments.first().filter(|name| !name.starts_with('-')) {
            assert!(stdout.contains(command_name), "{stdout}");
        }
    }

    let mut top_help = roused_command();
    top_help.arg("--help");
    let top_help = stdout(&run_to_exit(top_help));
    for expected in ["init-config", "check-config", "init-gateway-plist"] {
        assert!(top_help.contains(expected), "{top_help}");
    }

    let no_arguments = run_to_exit(roused_command());
    assert_exit_2(&no_arguments);
    let no_arguments_help = stderr(&no_arguments);
    for expected in ["init-config", "check-config", "init-gateway-plist"] {
        assert!(no_arguments_help.contains(expected), "{no_arguments_help}");
    }

    for arguments in [
        vec!["init-config"],
        vec!["init-config", "one.toml", "two.toml"],
        vec!["check-config"],
        vec!["check-config", "one.toml", "two.toml"],
        vec!["init-gateway-plist"],
        vec!["init-gateway-plist", "--unknown"],
    ] {
        let mut command = roused_command();
        command.args(arguments);
        let output = run_to_exit(command);
        assert_exit_2(&output);
        assert!(stderr(&output).trim().len() > "roused:".len());
    }
}

fn valid_configuration(listen: std::net::SocketAddr) -> String {
    format!(
        "listen = \"{listen}\"\n\n[[services]]\nhost = \"service.apps.test\"\nupstream = \"127.0.0.1:19090\"\nlaunchd_label = \"net.test.service\"\nidle_timeout_seconds = 1800\n"
    )
}

fn roused_command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_roused"))
}

fn run_init_gateway_plist(
    label: &str,
    config: &Path,
    output: &Path,
    program: Option<&Path>,
    current_directory: Option<&Path>,
) -> Output {
    let mut command = roused_command();
    command
        .arg("init-gateway-plist")
        .arg("--label")
        .arg(label)
        .arg("--config")
        .arg(config)
        .arg("--output")
        .arg(output);
    if let Some(program) = program {
        command.arg("--program").arg(program);
    }
    if let Some(current_directory) = current_directory {
        command.current_dir(current_directory);
    }
    run_to_exit(command)
}

fn run_to_exit(mut command: Command) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start roused command");
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        if child.try_wait().expect("poll roused command").is_some() {
            return child.wait_with_output().expect("collect roused output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().expect("collect timed-out output");
            panic!(
                "roused command did not exit in time\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed with {}\nstdout: {}\nstderr: {}",
        output.status,
        stdout(output),
        stderr(output)
    );
}

fn assert_exit_2(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(2),
        "unexpected status {}\nstdout: {}\nstderr: {}",
        output.status,
        stdout(output),
        stderr(output)
    );
    assert!(stderr(output).starts_with("roused:"), "{}", stderr(output));
}

fn assert_overwrite_diagnostic(output: &Output) {
    let diagnostic = stderr(output).to_ascii_lowercase();
    assert!(
        diagnostic.contains("exist") || diagnostic.contains("overwrite"),
        "{diagnostic}"
    );
}

fn assert_absolute_path_error(output: &Output, argument: &str) {
    assert_exit_2(output);
    let diagnostic = stderr(output).to_ascii_lowercase();
    assert!(diagnostic.contains(argument), "{diagnostic}");
    assert!(diagnostic.contains("absolute"), "{diagnostic}");
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn plist_raw(path: &Path, key: &str) -> String {
    let output = Command::new("/usr/bin/plutil")
        .args(["-extract", key, "raw", "-n", "-o", "-"])
        .arg(path)
        .output()
        .expect("extract plist value");
    assert!(
        output.status.success(),
        "cannot extract {key} from {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("plist value is UTF-8")
}

fn assert_plist_is_valid(path: &Path) {
    let output = Command::new("/usr/bin/plutil")
        .arg("-lint")
        .arg(path)
        .output()
        .expect("run plutil");
    assert!(
        output.status.success(),
        "{} is not a valid plist: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}
