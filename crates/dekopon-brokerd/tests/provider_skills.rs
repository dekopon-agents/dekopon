#![cfg(unix)]

#[test]
fn skill_help_distinguishes_the_destination_from_the_output_format() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dekopon-brokerd"))
        .args(["provider", "skills", "--help"])
        .output()
        .expect("run skill help");
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(help.contains("--out <DIR>"));
    assert!(help.contains("--output <OUTPUT>"));
    assert!(help.contains("json"));
}
