use std::{
    process::{Child, Command, Stdio},
    thread, time,
};

fn spawn_app(path: &str) -> Option<Child> {
    match Command::new(path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => {
            println!("tee_init: spawned {path}");
            Some(child)
        }
        Err(err) => {
            println!("tee_init: failed to spawn {path}: {err}");
            None
        }
    }
}

fn main() {
    println!("tee_init: starting");

    let vsock_mgr = spawn_app("/tee/vsock-manager");
    if vsock_mgr.is_none() {
        println!("tee_init: failed to start vsock-manager, aborting");
        return;
    }
    println!("tee_init: vsock-manager started");

    thread::sleep(time::Duration::from_secs(1));

    let ta = spawn_app("/tee/kybox-rs");
    if ta.is_some() {
        println!("tee_init: kybox started");
    } else {
        println!("tee_init: failed to start kybox");
    }

    // 两个都是后台任务，等它们结束
    let mut children = vec![("/tee/vsock-manager", vsock_mgr), ("/tee/ta", ta)];
    for (name, child) in &mut children {
        if let Some(c) = child {
            match c.wait() {
                Ok(status) => println!("tee_init: {name} exited with {status}"),
                Err(err) => println!("tee_init: failed to wait {name}: {err}"),
            }
        }
    }
}
