use redis::{Client, Commands};
use std::process::{Command, Stdio};

const ONE: &str = env!("CARGO_BIN_EXE_one");
const TWO: &str = env!("CARGO_BIN_EXE_two");

#[test]
fn one() {
    let redis_url = "redis://127.0.0.1/";
    let client = Client::open(redis_url).unwrap();
    let mut conn = client.get_connection().unwrap();
    // Initialize account balances.
    conn.set::<_, _, ()>("account1", 1000).unwrap();
    conn.set::<_, _, ()>("account2", 1000).unwrap();
    conn.set::<_, _, ()>("account3", 1000).unwrap();
    // Loads functions.
    redis_locking::setup(&client).unwrap();
    // Executes multiple instances of `one.rs` and `two.rs`.
    let ones = (0..10)
        .map(|_| {
            Command::new(ONE)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();
    let twos = (0..10)
        .map(|_| {
            Command::new(TWO)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();
    // Waits for all instances to finish.
    for one in ones {
        let output = one.wait_with_output().unwrap();
        println!("one output: {output:?}");
    }
    for two in twos {
        let output = two.wait_with_output().unwrap();
        println!("two output: {output:?}");
    }

    let balance1: i64 = conn.get("account1").unwrap();
    let balance2: i64 = conn.get("account2").unwrap();
    let balance3: i64 = conn.get("account3").unwrap();
    println!(
        "Final balances: account1 = {}, account2 = {}, account3 = {}",
        balance1, balance2, balance3
    );
    println!("Total balance: {}", balance1 + balance2 + balance3);
}