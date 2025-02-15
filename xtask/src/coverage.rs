use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::{fs, process};

use log::{debug, info};

use crate::errors::{Error, Result};
use crate::utils::*;
use crate::Config;

// https://doc.rust-lang.org/stable/rustc/instrument-coverage.html

#[derive(Debug, serde_derive::Deserialize)]
struct CargoTestMessageProfile {
    test: bool,
}

#[derive(Debug, serde_derive::Deserialize)]
struct CargoTestMessage {
    profile: CargoTestMessageProfile,
    filenames: Vec<PathBuf>,
}

pub(crate) fn coverage(config: &Config) -> Result<()> {
    let coverage_dir = config
        .coverage_dir
        .to_str()
        .expect("Path is not valid UTF-8");

    let llvm_cov_common_args: [&str; 10] = [
        "--Xdemangler",
        "rustfilt",
        "--ignore-filename-regex",
        r#"/\.cargo/registry/"#,
        "--ignore-filename-regex",
        r#"/rustc/"#,
        "--ignore-filename-regex",
        r#"/tests.rs$$"#,
        "--ignore-filename-regex",
        &format!("^{}/", coverage_dir),
    ];

    let rustc_flags = OsStr::new("-Cinstrument-coverage -Clink-dead-code");

    let coverage_common_env: [(&str, &OsStr); 4] = [
        ("RUST_BACKTRACE", OsStr::new("1")),
        ("CARGO_INCREMENTAL", OsStr::new("0")),
        ("RUSTFLAGS", rustc_flags),
        ("RUSTDOCFLAGS", rustc_flags),
    ];

    let coverage_common_args: [&str; 5] = [
        "test",
        "--workspace",
        "--tests",
        "--target-dir",
        coverage_dir,
    ];

    rustfilt_version(config)?;

    let sys_root = sys_root_of_toolchain(config)?;

    let mut result = find_executable_file(&sys_root, "llvm-profdata");
    if result.is_err() {
        info!("Installing component 'llvm-tools-preview'...");
        let args = ["--quiet", "component", "add", "llvm-tools-preview"];
        rustup(config, &args)?;

        result = find_executable_file(&sys_root, "llvm-profdata");
    }
    let llvm_profdata = result?;
    let llvm_cov = find_executable_file(&sys_root, "llvm-cov")?;

    fs::create_dir_all(&config.coverage_dir)
        .map_err(|r| Error::from_io_path("std::fs::create_dir_all", &config.coverage_dir, r))?;

    info!("Cleaning up old coverage files...");
    let profraw_files = list_files(&config.coverage_dir, "profraw")?;
    profraw_files.into_iter().for_each(|p| {
        let _ignored = fs::remove_file(&p);
    });

    let tests_paths = build_coverage_binaries(config, &coverage_common_env, &coverage_common_args)?;
    run_coverage_binaries(config, &coverage_common_env, &coverage_common_args)?;

    merge_coverage_profraw_files(config, &llvm_profdata)?;

    export_coverage_lcov(config, &llvm_cov, &llvm_cov_common_args, &tests_paths)?;
    export_coverage_html(config, &llvm_cov, &llvm_cov_common_args, &tests_paths)
}

fn rustfilt_version(config: &Config) -> Result<()> {
    let mut cmd = process::Command::new("rustfilt");
    cmd.stdout(process::Stdio::null()).arg("--version");

    let mut result = run_cmd(cmd, "rustfilt");
    if result.is_err() {
        info!("Installing 'rustfilt'...");
        cargo_command(config, "", &["--quiet", "install", "rustfilt"])?;

        let mut cmd = process::Command::new("rustfilt");
        cmd.stdout(process::Stdio::null()).arg("--version");
        result = run_cmd(cmd, "rustfilt");
    }
    result
}

fn build_coverage_binaries(
    config: &Config,
    common_env: &[(&str, &OsStr)],
    common_args: &[&str],
) -> Result<Vec<PathBuf>> {
    info!("Building coverage binaries...");

    let mut cmd = process::Command::new("cargo");
    cmd.current_dir(config.workspace_dir)
        .stdout(process::Stdio::piped())
        .envs(common_env.iter().map(|(k, v)| (k, v)))
        .env("LLVM_PROFILE_FILE", "/dev/null")
        .args(common_args)
        .args(["--no-run", "--message-format=json"]);

    debug!("Running: {:?}", cmd);
    let output = cmd
        .spawn()
        .map_err(|r| Error::from_io_path("std::process::Command::spawn", "cargo", r))?
        .wait_with_output()
        .map_err(|r| Error::from_io_path("std::process::Child::wait_with_output", "cargo", r))?;
    if output.status.success() {
        Ok(test_binaries_from_cargo_test_messages(&output.stdout))
    } else {
        Err(Error::CommandFailed { name: "cargo" })
    }
}

fn run_coverage_binaries(
    config: &Config,
    common_env: &[(&str, &OsStr)],
    common_args: &[&str],
) -> Result<()> {
    info!("Running coverage binaries...");

    let mut cmd = process::Command::new("cargo");
    cmd.current_dir(config.workspace_dir)
        .envs(common_env.iter().map(|(k, v)| (k, v)))
        .env("LLVM_PROFILE_FILE", &config.coverage_dir.join("%m.profraw"))
        .args(common_args);
    run_cmd(cmd, "cargo")
}

fn merge_coverage_profraw_files(config: &Config, llvm_profdata: &Path) -> Result<()> {
    info!("Merging coverage data...");

    let profraw_files = list_files(&config.coverage_dir, "profraw")?;

    let mut cmd = process::Command::new(llvm_profdata);
    cmd.args(["merge", "--sparse", "--output"])
        .arg(&config.coverage_profdata)
        .args(&profraw_files);
    run_cmd(cmd, "llvm-profdata")
}

fn export_coverage_lcov(
    config: &Config,
    llvm_cov: &Path,
    llvm_cov_common_args: &[&str],
    tests_paths: &[PathBuf],
) -> Result<()> {
    info!("Exporting coverage LCOV...");

    let lcov_path = config.coverage_dir.join("lcov.info");
    let lcov_info = File::create(&lcov_path)
        .map_err(|r| Error::from_io_path("std::fs::File::create", &lcov_path, r))?;

    let mut cmd = process::Command::new(llvm_cov);
    cmd.stdout(lcov_info)
        .args(["export", "--format", "lcov"])
        .args(llvm_cov_common_args)
        .arg("--instr-profile")
        .arg(&config.coverage_profdata);
    for path in tests_paths {
        cmd.arg("--object").arg(path);
    }
    run_cmd(cmd, "llvm-cov")
}

fn export_coverage_html(
    config: &Config,
    llvm_cov: &Path,
    llvm_cov_common_args: &[&str],
    tests_paths: &[PathBuf],
) -> Result<()> {
    info!("Exporting coverage HTML...");

    let mut cmd = process::Command::new(llvm_cov);
    cmd.args(["show", "--format", "html"])
        .args(["--show-line-counts-or-regions", "--show-instantiations"])
        .args(llvm_cov_common_args)
        .arg("--instr-profile")
        .arg(&config.coverage_profdata)
        .arg("--output-dir")
        .arg(&config.coverage_dir);
    for path in tests_paths {
        cmd.arg("--object").arg(path);
    }
    run_cmd(cmd, "llvm-cov")?;

    let mut cmd = process::Command::new("patch");
    cmd.current_dir(&config.coverage_dir)
        .arg("--input")
        .arg(&config.workspace_dir.join("coverage-style.css.patch"));
    run_cmd(cmd, "patch")
}

fn rustc_print_sysroot(config: &Config) -> Result<Vec<u8>> {
    let name = "rustc --print sysroot";

    let mut cmd = process::Command::new("rustc");
    cmd.current_dir(config.workspace_dir)
        .stdout(process::Stdio::piped())
        .args(["--print", "sysroot"]);

    debug!("Running: {:?}", cmd);
    let output = cmd
        .spawn()
        .map_err(|r| Error::from_io_path("std::process::Command::spawn", name, r))?
        .wait_with_output()
        .map_err(|r| Error::from_io_path("std::process::Child::wait_with_output", name, r))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(Error::CommandFailed { name })
    }
}

fn sys_root_of_toolchain(config: &Config) -> Result<PathBuf> {
    let mut bytes = rustc_print_sysroot(config)?;
    if let Some(line_len) = bytes
        .as_slice()
        .split(|&c| c == b'\n' || c == b'\r')
        .next()
        .map(|s| s.len())
    {
        bytes.resize(line_len, 0); // Keep only the first line.
    }

    Ok(pathbuf_from_vec(bytes))
}

fn test_binaries_from_cargo_test_messages(bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|&c| c == b'\r' || c == b'\n')
        .map(serde_json::from_slice::<CargoTestMessage>)
        .filter_map(std::result::Result::ok)
        .filter(|obj| obj.profile.test)
        .flat_map(|obj| obj.filenames)
        .collect()
}
