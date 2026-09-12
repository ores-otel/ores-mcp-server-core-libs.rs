//! Exercise process-owned environment configuration without mutating test globals.

use ores_mcp_server_core_libs::transport::{
    HttpTransportConfig, TcpTransportConfig, WebSocketTransportConfig,
};
#[cfg(unix)]
use std::ffi::OsString;
use std::process::Command;
use std::time::Duration;

fn verify_environment_probe(probe: &str) {
    match probe {
        "defaults" => {
            let http = HttpTransportConfig::from_env().expect("HTTP defaults");
            let tcp = TcpTransportConfig::from_env().expect("TCP defaults");
            let ws = WebSocketTransportConfig::from_env().expect("WebSocket defaults");
            assert_eq!(http.bind, HttpTransportConfig::default().bind);
            assert_eq!(tcp.bind, TcpTransportConfig::default().bind);
            assert_eq!(ws.bind, WebSocketTransportConfig::default().bind);
            assert!(!http.allow_remote && !tcp.allow_remote && !ws.allow_remote);
        }
        "overrides" => {
            let http = HttpTransportConfig::from_env().expect("HTTP overrides");
            let tcp = TcpTransportConfig::from_env().expect("TCP overrides");
            let ws = WebSocketTransportConfig::from_env().expect("WebSocket overrides");
            assert_eq!(http.bind.to_string(), "0.0.0.0:3101");
            assert_eq!(tcp.bind.to_string(), "0.0.0.0:3102");
            assert_eq!(ws.bind.to_string(), "0.0.0.0:3103");
            assert!(http.allow_remote && tcp.allow_remote && ws.allow_remote);
            assert_eq!(http.allowed_hosts, ["api.example:3101", "localhost:3101"]);
            assert_eq!(http.allowed_origins, ["https://console.example"]);
            assert_eq!(ws.allowed_hosts, ["socket.example:3103"]);
            assert_eq!(ws.allowed_origins, ["https://console.example"]);
            assert_eq!(http.max_body_bytes, 512);
            assert_eq!((tcp.max_connections, tcp.max_message_bytes), (7, 1024));
            assert_eq!((ws.max_connections, ws.max_message_bytes), (9, 2048));
            assert_eq!(http.shutdown_grace, Duration::from_secs(3));
            assert_eq!(tcp.shutdown_grace, Duration::from_secs(4));
            assert_eq!(ws.shutdown_grace, Duration::from_secs(5));
        }
        "http-reject" => {
            let error = HttpTransportConfig::from_env().expect_err("reject unsafe HTTP input");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(!error.to_string().contains("synthetic-sensitive-value"));
        }
        "tcp-reject" => {
            assert_eq!(
                TcpTransportConfig::from_env()
                    .expect_err("reject unsafe TCP input")
                    .kind(),
                std::io::ErrorKind::InvalidInput
            );
        }
        "ws-reject" => {
            assert_eq!(
                WebSocketTransportConfig::from_env()
                    .expect_err("reject unsafe WebSocket input")
                    .kind(),
                std::io::ErrorKind::InvalidInput
            );
        }
        _ => panic!("unknown test probe"),
    }
}

#[test]
fn process_environment_admission() {
    if let Ok(probe) = std::env::var("ORES_TEST_TRANSPORT_PROBE") {
        verify_environment_probe(&probe);
        return;
    }

    let cases: &[(&str, &[(&str, &str)])] = &[
        ("defaults", &[]),
        (
            "overrides",
            &[
                ("MCP_HTTP_BIND", "0.0.0.0:3101"),
                ("MCP_HTTP_ALLOW_REMOTE", "TRUE"),
                (
                    "MCP_HTTP_ALLOWED_HOSTS",
                    " api.example:3101, localhost:3101 ",
                ),
                ("MCP_HTTP_ALLOWED_ORIGINS", "https://console.example"),
                ("MCP_HTTP_MAX_BODY_BYTES", "512"),
                ("MCP_HTTP_SHUTDOWN_GRACE_SECONDS", "3"),
                ("MCP_TCP_BIND", "0.0.0.0:3102"),
                ("MCP_TCP_ALLOW_REMOTE", "1"),
                ("MCP_TCP_MAX_CONNECTIONS", "7"),
                ("MCP_TCP_MAX_MESSAGE_BYTES", "1024"),
                ("MCP_TCP_SHUTDOWN_GRACE_SECONDS", "4"),
                ("MCP_WS_BIND", "0.0.0.0:3103"),
                ("MCP_WS_ALLOW_REMOTE", "true"),
                ("MCP_WS_ALLOWED_HOSTS", "socket.example:3103"),
                ("MCP_WS_ALLOWED_ORIGINS", "https://console.example"),
                ("MCP_WS_MAX_CONNECTIONS", "9"),
                ("MCP_WS_MAX_MESSAGE_BYTES", "2048"),
                ("MCP_WS_SHUTDOWN_GRACE_SECONDS", "5"),
            ],
        ),
        ("http-reject", &[("MCP_HTTP_BIND", "0.0.0.0:3101")]),
        (
            "http-reject",
            &[("MCP_HTTP_ALLOW_REMOTE", "synthetic-sensitive-value")],
        ),
        (
            "http-reject",
            &[("MCP_HTTP_MAX_BODY_BYTES", "synthetic-sensitive-value")],
        ),
        ("http-reject", &[("MCP_HTTP_SHUTDOWN_GRACE_SECONDS", "0")]),
        ("tcp-reject", &[("MCP_TCP_MAX_CONNECTIONS", "0")]),
        (
            "tcp-reject",
            &[("MCP_TCP_MAX_MESSAGE_BYTES", "18446744073709551616")],
        ),
        (
            "tcp-reject",
            &[
                ("MCP_TCP_BIND", "0.0.0.0:3102"),
                ("MCP_TCP_ALLOW_REMOTE", "false"),
            ],
        ),
        ("ws-reject", &[("MCP_WS_ALLOWED_HOSTS", " , , ")]),
        (
            "ws-reject",
            &[("MCP_WS_ALLOWED_ORIGINS", "https://console.example/private")],
        ),
        (
            "ws-reject",
            &[
                ("MCP_WS_BIND", "0.0.0.0:3103"),
                ("MCP_WS_ALLOW_REMOTE", "0"),
            ],
        ),
    ];
    for (probe, environment) in cases {
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", "process_environment_admission", "--nocapture"])
            .env_clear()
            .env("ORES_TEST_TRANSPORT_PROBE", probe)
            .envs(environment.iter().copied())
            .output()
            .expect("execute isolated environment probe");
        assert!(
            output.status.success(),
            "{probe}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", "process_environment_admission", "--nocapture"])
            .env_clear()
            .env("ORES_TEST_TRANSPORT_PROBE", "http-reject")
            .env("MCP_HTTP_BIND", OsString::from_vec(vec![0xff]))
            .output()
            .expect("execute non-Unicode environment probe");
        assert!(
            output.status.success(),
            "non-Unicode configuration must fail closed"
        );
    }
}
