use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::process::ChildStderr;
use std::sync::mpsc::{channel, Receiver, Sender};
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

#[derive(Serialize, Clone)]
struct ResultStruct {
    exit: String,
    is_plus: bool,
    time_to_connect: u128,
    data_error: DataOrError,
    geph_stderr: String,
}
#[derive(Serialize, Clone)]
enum DataOrError {
    Data(HashMap<String, Vec<MeasurementStruct>>),
    Error(String, u64),
}

#[derive(Serialize, Clone, Copy)]
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
        let (mut child, exit, is_plus, receiver) =
            connect_to_geph(username.clone(), password.clone());
        scopeguard::defer!({
            //let pid = child.id();
            child.kill().unwrap();
            child.wait().unwrap();
            //eprintln!("KILLLLLED!!!!! pid = {}", pid);
        });
        let time_to_connect = start.elapsed().as_millis();
        println!("connecting to geph took {} millisecs", time_to_connect);

        let mut result_struct = ResultStruct {
            exit,
            is_plus,
            time_to_connect,
            data_error: DataOrError::Data(HashMap::new()),
            geph_stderr: String::new(),
        };

        let config_url =
            "https://raw.githubusercontent.com/cheekyelf/geph-autotest/main/config.toml";

        // Fetch testing configuration document into a hashmap
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all("http://localhost:10910")?)
            .build()?;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let config_file = rt.block_on(async_download(&client, config_url))?;
        let config: Config = toml::from_slice(&config_file).context("cannot parse TOML file")?;
        // print!("{:?}\n", config);
        // Perform each test
        for (name, td) in config.endpoints.into_iter() {
            println!(
                "downloading test file \"{}\" {} time(s) (millisecs)",
                name, td.iterations
            );

            let mut result_vec: Vec<MeasurementStruct> = Vec::new();
            for _ in 0..td.iterations {
                let duration =
                    measure_time(|| rt.block_on(async_download_no_res(&client, &td.url)));
                match duration {
                    Ok(dur) => {
                        println!("{}", dur.as_millis());
                        result_vec.push(MeasurementStruct {
                            download_time: dur.as_millis(),
                            timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
                        });
                    }
                    Err(e) => {
                        println!("an error was encountered! {:?}", e);
                        result_struct.data_error = DataOrError::Error(
                            format!("{:?}", e),
                            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
                        );
                        break;
                    }
                }
                // Wait a random number of seconds that averages to avg_indi
                std::thread::sleep(Duration::from_secs(fastrand::u64(0..=(td.interval * 2))));
            }
            match &mut result_struct.data_error {
                DataOrError::Data(datamap) => {
                    datamap.insert(name, result_vec);
                }
                DataOrError::Error(_, _) => {
                    break;
                }
            }
        }
        // Add geph's stderr logs
        while let Ok(line) = receiver.try_recv() {
            println!("{}", line);
            result_struct.geph_stderr.push_str(line.as_str());
        }

        // println!("{}", result_struct.geph_stderr);

        // Send results to data aggregation server
        let json_str =
            serde_json::to_string(&result_struct).context("could not serialize result_struct")?;

        ureq::post(&config.collector).send_string(&json_str)?;
        // writeln!(results_file, "{}", json_str).context("could not write result to file")?;
        println!("uploaded test results!\n");
        // Wait a random number of seconds that averages to avg_total then re-test
        std::thread::sleep(Duration::from_secs(fastrand::u64(
            0..=(config.global_interval * 2),
        )));
    }
}

async fn async_download(client: &reqwest::Client, src: &str) -> anyhow::Result<Vec<u8>> {
    let file = client
        .get(src)
        .timeout(Duration::from_secs(60))
        .send()
        .await?
        .bytes()
        .await?;
    Ok(file.to_vec())
}

async fn async_download_no_res(client: &reqwest::Client, src: &str) -> anyhow::Result<()> {
    let mut res = client.get(src).send().await?;
    while res.chunk().await?.is_some() {}
    Ok(())
}

// Connects to Geph and returns when connection is established
fn connect_to_geph(username: String, password: String) -> (Child, String, bool, Receiver<String>) {
    // Retrieve a list of all geph exits
    let output = Command::new("geph4-client")
        .arg("sync")
        .arg("--username")
        .arg(username.clone())
        .arg("--password")
        .arg(password.clone())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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
    // println!("\npicked our exit!");

    // Connect to Geph with our exit
    loop {
        let mut child = Command::new("geph4-client")
            .arg("connect")
            .arg("--username")
            .arg(&username)
            .arg("--password")
            .arg(&password)
            .arg("--exit-server")
            .arg(&exit.hostname)
            .arg("--http-listen")
            .arg("127.0.0.1:10910")
            .arg("--socks5-listen")
            .arg("127.0.0.1:10909")
            .arg("--stats-listen")
            .arg("127.0.0.1:10809")
            .arg("--credential-cache")
            .arg("/tmp/manual")
            .stderr(Stdio::piped())
            .spawn()
            .expect("could not connect to geph");
        //eprintln!("STARTING CHILD WITH PID = {}", child.id());

        let stderr = child.stderr.take().expect("could not get child stderr");
        let mut stderr = BufReader::new(stderr);
        let mut line = String::new();
        let (sender, receiver) = channel();
        loop {
            line.clear();
            let n = stderr
                .read_line(&mut line)
                .expect("could not read from child stderr");
            sender.send(line.clone()).unwrap();

            if n == 0 {
                eprintln!("OH NO RETRYING!!!!!!");
                // child.kill().unwrap();
                child.wait().unwrap();
                std::thread::sleep(Duration::from_secs(1));
                break;
            };
            //dbg!(&line);
            if line.contains("TUNNEL_MANAGER MAIN LOOP") {
                std::thread::spawn(move || send_stderr(stderr, sender));
                return (child, exit.hostname, is_plus, receiver);
            }
        }
    }
}

// std::io::copy(&mut stderr, &mut std::io::sink())

fn send_stderr(mut stderr: BufReader<ChildStderr>, sender: Sender<String>) {
    let mut line = String::new();
    loop {
        line.clear();
        match stderr.read_line(&mut line) {
            Ok(0) => {
                return;
            }
            Ok(_) => {
                sender.send(line.clone()).unwrap();
            }
            Err(e) => {
                panic!("{}", e);
            }
        }
    }
}

// // Runs a command and returns the stdout
// fn run(command: &str) -> anyhow::Result<Vec<u8>> {
//     let mut child = Command::new("sh")
//         .arg("-c")
//         .arg(command)
//         .stdout(Stdio::piped())
//         .stderr(Stdio::null())
//         .spawn()?;
//     // eprintln!("\nrunning command {}", command);
//     let status = child.wait()?;

//     if status.success() {
//         let output = child.wait_with_output()?;
//         Ok(output.stdout)
//     } else {
//         Err(anyhow!(format!(
//             "command {} exited with status {}!",
//             command, status
//         )))
//     }
// }

fn measure_time(f: impl FnOnce() -> Result<(), anyhow::Error>) -> Result<Duration, anyhow::Error> {
    let start = Instant::now();
    f().context("could not download test file")?;
    Ok(start.elapsed())
}
