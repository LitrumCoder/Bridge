#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use bridge_ipc::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_and_server_talk_over_the_os_endpoint() {
    let endpoint = if cfg!(windows) {
        format!("bridge-test-{}", std::process::id())
    } else {
        std::env::temp_dir()
            .join(format!("bridge-test-{}", std::process::id()))
            .join("ipc.sock")
            .to_string_lossy()
            .into_owned()
    };
    let server = IpcServer::bind(&endpoint).unwrap();
    let handler: IpcHandler = Arc::new(|req| {
        Box::pin(async move {
            match req {
                Request::Devices => Response::Devices(Vec::new()),
                Request::Shutdown => Response::Ok,
                other => Response::Error(format!("unsupported: {other:?}")),
            }
        })
    });
    let task = tokio::spawn(server.serve(handler));

    let mut client = IpcClient::connect(&endpoint).await.unwrap();
    assert_eq!(
        client.request(Request::Devices).await.unwrap(),
        Response::Devices(Vec::new())
    );
    assert_eq!(
        client.request(Request::Shutdown).await.unwrap(),
        Response::Ok
    );
    assert!(matches!(
        client.request(Request::Status).await.unwrap(),
        Response::Error(_)
    ));
    task.abort();
}
