//! Native subprocess fixture: no torrent engine, network or third-party crates.
use std::io::{Read, Write};
fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 4);
    assert_eq!(args[1], "request");
    assert_eq!(args[2], "--profile");
    let token = std::env::var("PEERLUME_CONTROL_TOKEN").unwrap();
    assert_eq!(token, "synthetic-control-credential");
    assert!(!args.iter().any(|arg| arg.contains(&token)));
    let profile = std::path::Path::new(&args[3]);
    assert!(profile.is_absolute());
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    assert!(input.ends_with('\n'));
    assert!(!input.contains(&token));
    let mode = std::fs::read_to_string(profile.join("mode")).unwrap();
    match mode.as_str() {
        "echo" => println!("{{\"protocol_version\":1,\"instance_id\":\"fixture-owner\",\"result\":{input},\"error\":null}}"),
        "denied" => {
            eprintln!("Untrusted stderr must remain hidden: {token}");
            println!("{{\"protocol_version\":1,\"instance_id\":\"\",\"result\":null,\"error\":\"permission_denied\"}}");
            std::process::exit(4);
        }
        "sleep" => std::thread::sleep(std::time::Duration::from_secs(30)),
        "stdout_limit" => std::io::stdout().write_all(&vec![b'x';300_000]).unwrap(),
        "stderr_limit" => std::io::stderr().write_all(&vec![b'x';30_000]).unwrap(),
        "invalid" => println!("Invalid response containing {token}"),
        _ => panic!("Unknown fixture mode"),
    }
}
