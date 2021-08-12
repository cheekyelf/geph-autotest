use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{self, BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use structopt::StructOpt;

#[derive(StructOpt)]
struct UserInfo {
    #[structopt(long)]
    username: Option<String>,
    #[structopt(long)]
    password: Option<String>,
}

#[derive(Deserialize, Debug)]
struct Config {
    collector: String,                          // where to send test data
    global_interval: u64, // how many seconds to wait between running all tests
    endpoints: HashMap<String, TestDescriptor>, // what to do in each particular test
}

#[derive(Deserialize, Debug)]
struct TestDescriptor {
    url: String,     // what to download
    iterations: u32, // how many times to download
    interval: u64,   // how many seconds to wait after each download
}

#[derive(Serialize)]
struct ResultStruct {
    exit: String,
    is_plus: bool,
    time_to_connect: u128,
    data: HashMap<String, Vec<MeasurementStruct>>,
}

#[derive(Serialize)]
struct MeasurementStruct {
    download_time: u128,
    timestamp: u64,
}

fn prompt_to_input(prompt: &str) -> String {
    let stdin = io::stdin();
    let mut ret = String::new();

    print!("{}", prompt);
    io::stdout().flush().unwrap();

    stdin.read_line(&mut ret).unwrap();
    ret.trim().to_string()
}

fn main() -> anyhow::Result<()> {
    // // Create file for storing test results
    // let mut results_file = File::create(format!(
    //     "testing_stats_{}",
    //     (SystemTime::now().duration_since(UNIX_EPOCH))
    //         .unwrap()
    //         .as_secs()
    // ))
    // .context("could not create results file")?;

    // Get username & password
    let userinfo = UserInfo::from_args();

    let username = userinfo
        .username
        .unwrap_or_else(|| prompt_to_input("Enter your username: "));

    let password = userinfo
        .password
        .unwrap_or_else(|| prompt_to_input("Enter your password: "));

    std::env::set_var("GEPH_RECURSIVE", "1");

    loop {
        // Connect to Geph & log exit chosen & time taken to connect
        let start = Instant::now();
        let (mut child, exit, is_plus) = connect_to_geph(username.clone(), password.clone());
        scopeguard::defer!({
            let pid = child.id();
            child.kill().unwrap();
            child.wait().unwrap();
            eprintln!("KILLLLLED!!!!! pid = {}", pid);
        });
        let time_to_connect = start.elapsed().as_millis();

        let mut result_struct = ResultStruct {
            exit,
            is_plus,
            time_to_connect,
            data: HashMap::new(),
        };

        // Fetch testing configuration document into a hashmap
        let config_file =
            run("curl --proxy socks5h://localhost:10909 https://raw.githubusercontent.com/cheekyelf/geph-autotest/main/config.toml")
                .context("could not get config file")?;
        let config: Config = toml::from_slice(&config_file).context("cannot parse TOML file")?;

        // Perform each test
        for (name, td) in config.endpoints.into_iter() {
            let mut result_vec: Vec<MeasurementStruct> = Vec::new();

            for _ in 0..=td.iterations {
                let duration = measure_time(|| {
                    run(&format!(
                        "curl --proxy socks5h://localhost:10909 {}> /dev/null",
                        td.url
                    ))
                })
                .context("could not measure download time")?;
                // Question: if run() fails, would "could not measure download time" be displayed in the logs too?
                result_vec.push(MeasurementStruct {
                    download_time: duration.as_millis(),
                    timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
                });

                // Wait a random number of seconds that averages to avg_indi
                std::thread::sleep(Duration::from_secs(fastrand::u64(0..=(td.interval * 2))));
            }
            result_struct.data.insert(name, result_vec);
        }

        // Send result to data aggregation server
        let json_str =
            serde_json::to_string(&result_struct).context("could not serialize result_struct")?;

        ureq::post(&config.collector).send_string(&json_str)?;
        // writeln!(results_file, "{}", json_str).context("could not write result to file")?;

        // Wait a random number of seconds that averages to avg_total then re-test
        std::thread::sleep(Duration::from_secs(fastrand::u64(
            0..=(config.global_interval * 2),
        )));
    }
}

// Connects to Geph and returns when connection is established
fn connect_to_geph(username: String, password: String) -> (Child, String, bool) {
    // Retrieve a list of all geph exits
    let output = Command::new("geph4-client")
        .arg("sync")
        .arg("--username")
        .arg(username.clone())
        .arg("--password")
        .arg(password.clone())
        // .arg("--http-listen")
        // .arg("10910")
        // .arg("--socks5-listen")
        // .arg("10909")
        // .arg("--stats-listen")
        // .arg("10809")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawning geph4-client failed");

    let raw_output = output.wait_with_output().expect("failed to wait");
    let stdout = raw_output.stdout;
    // println!("{}", std::str::from_utf8(&stdout).unwrap());

    let deserialized: Vec<serde_json::Value> =
        serde_json::from_slice(&stdout).expect("geph gave us bad json");

    // Check if our testing user is a plus user
    #[derive(Deserialize, Debug, Clone)]
    struct SubscriptionInfo {
        subscription: Option<serde_json::Value>,
    }
    let mut is_plus = false;
    let subscription_info: SubscriptionInfo =
        serde_json::from_value(deserialized[0].clone()).expect("could not deserialize user info");
    if subscription_info.subscription.is_some() {
        is_plus = true;
    }

    #[derive(Deserialize, Debug, Clone)]
    struct ExitDescriptor {
        hostname: String,
    }
    let exit_list: Vec<ExitDescriptor> =
        serde_json::from_value(deserialized[if is_plus { 1 } else { 2 }].clone())
            .expect("could not deserialize bridges");

    // Randomly pick an exit
    let exit = exit_list[fastrand::usize(..exit_list.len())].clone();
    loop {
        // Connect to Geph with our exit
        let mut child = Command::new("sh")
        .arg("-c")
        .arg(&format!(
            "geph4-client connect --username {} --password {} --exit-server {} --http-listen 127.0.0.1:10910 --socks5-listen 127.0.0.1:10909 --stats-listen 127.0.0.1:10809",
            username, password, exit.hostname
        ))
        .stderr(Stdio::piped())
        .spawn()
        .expect("could not connect to geph");

        let stderr = child.stderr.take().expect("could not get child stderr");
        let mut stderr = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            let n = stderr
                .read_line(&mut line)
                .expect("could not read from child stderr");
            if n == 0 {
                eprintln!("OH NO RETRYING!!!!!!");
                // child.kill().unwrap();
                child.wait().unwrap();
                break;
            }
            dbg!(&line);
            if line.contains("TUNNEL_MANAGER MAIN LOOP") {
                std::thread::spawn(move || std::io::copy(&mut stderr, &mut std::io::sink()));
                return (child, exit.hostname, is_plus);
            }
        }
    }
}

// Runs a command and returns the stdout
fn run(command: &str) -> anyhow::Result<Vec<u8>> {
    let child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        // .stderr(Stdio::null())
        .spawn()?;
    eprintln!("running command {}", command);
    let output = child.wait_with_output()?;

    return Ok(output.stdout);
}

fn measure_time(
    f: impl FnOnce() -> Result<Vec<u8>, anyhow::Error>,
) -> Result<Duration, anyhow::Error> {
    let start = Instant::now();
    f().context("could not download test file")?;
    Ok(start.elapsed())
}
