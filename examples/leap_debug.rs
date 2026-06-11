//! Temporary LeapC connection debugger: prints every poll result.
//! Run: cargo run --features leap --example leap_debug

#[cfg(feature = "leap")]
fn main() {
    use leaprs::{Connection, ConnectionConfig, EventRef};

    let mut connection =
        Connection::create(ConnectionConfig::default()).expect("create connection");
    connection.open().expect("open connection");
    println!("opened, polling...");

    for i in 0..200 {
        match connection.poll(1000) {
            Ok(message) => {
                let name = match message.event() {
                    EventRef::None => "None",
                    EventRef::Connection(_) => "Connection",
                    EventRef::ConnectionLost(_) => "ConnectionLost",
                    EventRef::Device(_) => "Device",
                    EventRef::DeviceLost => "DeviceLost",
                    EventRef::DeviceFailure(_) => "DeviceFailure",
                    EventRef::Policy(_) => "Policy",
                    EventRef::Tracking(e) => {
                        println!("{i}: Tracking, hands={}", e.hands().len());
                        continue;
                    }
                    EventRef::LogEvent(_) => "LogEvent",
                    EventRef::LogEvents(_) => "LogEvents",
                    _ => "Other",
                };
                println!("{i}: {name}");
            }
            Err(err) => println!("{i}: ERR {err:?}"),
        }
    }
}

#[cfg(not(feature = "leap"))]
fn main() {
    eprintln!("build with --features leap");
}
