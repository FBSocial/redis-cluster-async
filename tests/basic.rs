use std::{
    cell::Cell,
    error::Error,
    sync::{Mutex, MutexGuard},
};

use {
    futures::{prelude::*, stream},
    proptest::proptest,
    tokio::runtime::Runtime,
};

use redis_cluster_async::{
    redis::{cmd, AsyncCommands, RedisError, RedisResult, Script},
    Client,
};

const REDIS_URL: &str = "redis://127.0.0.1:7000/";

pub struct RedisProcess;
pub struct RedisLock(MutexGuard<'static, RedisProcess>);

impl RedisProcess {
    // Blocks until we have sole access.
    pub fn lock() -> RedisLock {
        lazy_static::lazy_static! {
            static ref REDIS: Mutex<RedisProcess> = Mutex::new(RedisProcess {});
        }

        // If we panic in a test we don't want subsequent to fail because of a poisoned error
        let redis_lock = REDIS
            .lock()
            .unwrap_or_else(|poison_error| poison_error.into_inner());
        RedisLock(redis_lock)
    }
}

// ----------------------------------------------------------------------------

pub struct RuntimeEnv {
    pub redis: RedisEnv,
    pub runtime: Runtime,
}

impl RuntimeEnv {
    pub fn new() -> Self {
        let mut runtime = tokio::runtime::Builder::new()
            .basic_scheduler()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();
        let redis = runtime.block_on(RedisEnv::new());
        Self { runtime, redis }
    }
}
pub struct RedisEnv {
    _redis_lock: RedisLock,
    pub client: Client,
    nodes: Vec<redis::aio::MultiplexedConnection>,
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl RedisEnv {
    pub async fn new() -> Self {
        let _ = env_logger::try_init();

        let redis_lock = RedisProcess::lock();

        let redis_client = redis::Client::open(REDIS_URL)
            .unwrap_or_else(|_| panic!("Failed to connect to '{}'", REDIS_URL));

        let mut master_urls = Vec::new();
        let mut nodes = Vec::new();

        'outer: loop {
            let node_infos = async {
                let mut conn = redis_client.get_multiplexed_tokio_connection().await?;
                Self::cluster_info(&mut conn).await
            }
            .await
            .expect("Unable to query nodes for information");
            // Wait for the cluster to stabilize
            if node_infos.iter().filter(|(_, master)| *master).count() == 3 {
                let cleared_nodes = async {
                    master_urls.clear();
                    nodes.clear();
                    // Clear databases:
                    for (url, master) in node_infos {
                        let redis_client = redis::Client::open(&url[..])
                            .unwrap_or_else(|_| panic!("Failed to connect to '{}'", url));
                        let mut conn = redis_client.get_multiplexed_tokio_connection().await?;

                        if master {
                            master_urls.push(url.to_string());
                            let () = tokio::time::timeout(
                                std::time::Duration::from_secs(3),
                                redis::Cmd::new()
                                    .arg("FLUSHALL")
                                    .query_async(&mut conn)
                                    .map_err(BoxError::from),
                            )
                            .await
                            .unwrap_or_else(|err| Err(BoxError::from(err)))?;
                        }

                        nodes.push(conn);
                    }
                    Ok::<_, BoxError>(())
                }
                .await;
                match cleared_nodes {
                    Ok(()) => break 'outer,
                    Err(err) => {
                        // Failed to clear the databases, retry
                        log::warn!("{}", err);
                    }
                }
            }
            tokio::time::delay_for(std::time::Duration::from_millis(100)).await;
        }

        let client = Client::open(master_urls.iter().map(|s| &s[..]).collect()).unwrap();

        RedisEnv {
            client,
            nodes,
            _redis_lock: redis_lock,
        }
    }

    async fn cluster_info<T>(redis_client: &mut T) -> RedisResult<Vec<(String, bool)>>
    where
        T: Clone + redis::aio::ConnectionLike + Send + 'static,
    {
        redis::cmd("CLUSTER")
            .arg("NODES")
            .query_async(redis_client)
            .map_ok(|s: String| {
                s.lines()
                    .map(|line| {
                        let mut iter = line.split(' ');
                        let port = iter
                            .by_ref()
                            .nth(1)
                            .expect("Node ip")
                            .splitn(2, '@')
                            .next()
                            .unwrap()
                            .splitn(2, ':')
                            .nth(1)
                            .unwrap();
                        (
                            format!("redis://localhost:{}", port),
                            iter.next().expect("master").contains("master"),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .await
    }
}

#[tokio::test]
async fn basic_cmd() {
    let env = RedisEnv::new().await;
    let client = env.client;
    async {
        let mut connection = client.get_connection().await?;
        let () = cmd("SET")
            .arg("test")
            .arg("test_data")
            .query_async(&mut connection)
            .await?;
        let res: String = cmd("GET")
            .arg("test")
            .clone()
            .query_async(&mut connection)
            .await?;
        assert_eq!(res, "test_data");
        Ok(())
    }
    .await
    .map_err(|err: RedisError| err)
    .unwrap()
}

#[tokio::test]
async fn basic_eval() {
    let env = RedisEnv::new().await;
    let client = env.client;
    async {
        let mut connection = client.get_connection().await?;
        let res: String = cmd("EVAL")
            .arg(r#"redis.call("SET", KEYS[1], ARGV[1]); return redis.call("GET", KEYS[1])"#)
            .arg(1)
            .arg("key")
            .arg("test")
            .query_async(&mut connection)
            .await?;
        assert_eq!(res, "test");
        Ok(())
    }
    .await
    .map_err(|err: RedisError| err)
    .unwrap()
}

#[ignore] // TODO Handle running SCRIPT LOAD on all masters
#[tokio::test]
async fn basic_script() {
    let env = RedisEnv::new().await;
    let client = env.client;
    async {
        let mut connection = client.get_connection().await?;
        let res: String = Script::new(
            r#"redis.call("SET", KEYS[1], ARGV[1]); return redis.call("GET", KEYS[1])"#,
        )
        .key("key")
        .arg("test")
        .invoke_async(&mut connection)
        .await?;
        assert_eq!(res, "test");
        Ok(())
    }
    .await
    .map_err(|err: RedisError| err)
    .unwrap()
}

#[ignore] // TODO Handle pipe where the keys do not all go to the same node
#[tokio::test]
async fn basic_pipe() {
    let env = RedisEnv::new().await;
    let client = env.client;
    async {
        let mut connection = client.get_connection().await?;
        let mut pipe = redis::pipe();
        pipe.add_command(cmd("SET").arg("test").arg("test_data").clone());
        pipe.add_command(cmd("SET").arg("test3").arg("test_data3").clone());
        let () = pipe.query_async(&mut connection).await?;
        let res: String = connection.get("test").await?;
        assert_eq!(res, "test_data");
        let res: String = connection.get("test3").await?;
        assert_eq!(res, "test_data3");
        Ok(())
    }
    .await
    .map_err(|err: RedisError| err)
    .unwrap()
}

#[test]
fn proptests() {
    let env = std::cell::RefCell::new(FailoverEnv::new());

    proptest!(
        proptest::prelude::ProptestConfig { cases: 30, failure_persistence: None, .. Default::default() },
        |(requests in 0..15, value in 0..i32::max_value())| {
            test_failover(&mut env.borrow_mut(), requests, value)
        }
    );
}

#[test]
fn basic_failover() {
    test_failover(&mut FailoverEnv::new(), 10, 123);
}

struct FailoverEnv {
    env: RuntimeEnv,
    connection: redis_cluster_async::Connection,
}

impl FailoverEnv {
    fn new() -> Self {
        let mut env = RuntimeEnv::new();
        let connection = env
            .runtime
            .block_on(env.redis.client.get_connection())
            .unwrap();

        FailoverEnv { env, connection }
    }
}

async fn do_failover(
    redis: &mut redis::aio::MultiplexedConnection,
) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    cmd("CLUSTER")
        .arg("FAILOVER")
        .query_async(redis)
        .err_into()
        .await
}

fn test_failover(env: &mut FailoverEnv, requests: i32, value: i32) {
    let completed = Cell::new(0);
    let completed = &completed;

    let FailoverEnv { env, connection } = env;

    let nodes = env.redis.nodes.clone();

    let test_future = async {
        (0..requests + 1)
            .map(|i| {
                let mut connection = connection.clone();
                let mut nodes = nodes.clone();
                async move {
                    if i == requests / 2 {
                        // Failover all the nodes, error only if all the failover requests error
                        nodes.iter_mut().map(|node| do_failover(node))
                            .collect::<stream::FuturesUnordered<_>>()
                            .fold(
                                Err(Box::<dyn Error + Send + Sync>::from("None".to_string())),
                                |acc: Result<(), Box<dyn Error + Send + Sync>>,
                                 result: Result<(), Box<dyn Error + Send + Sync>>| async move {
                                    acc.or_else(|_| result)
                                },
                            )
                            .await
                    } else {
                        let key = format!("test-{}-{}", value, i);
                        let () = cmd("SET")
                            .arg(&key)
                            .arg(i)
                            .clone()
                            .query_async(&mut connection)
                            .await?;
                        let res: i32 = cmd("GET")
                            .arg(key)
                            .clone()
                            .query_async(&mut connection)
                            .await?;
                        assert_eq!(res, i);
                        completed.set(completed.get() + 1);
                        Ok(())
                    }
                }
            })
            .collect::<stream::FuturesUnordered<_>>()
            .try_collect()
            .await
    };
    env.runtime
        .block_on(test_future)
        .unwrap_or_else(|err| panic!("{}", err));
    assert_eq!(completed.get(), requests, "Some requests never completed!");
}

#[tokio::test]
async fn test_xgroup_stream() {
    let nodes = vec!["redis://10.100.1.36:6380/", "redis://10.100.1.36:6381/", "redis://10.100.1.36:6382/"];

    let mut client = Client::open(nodes).unwrap();
    client.set_password("idreamsky@123");
    let mut connnection = client.get_connection().map_err(|err|{
        println!("get connection failed with err={}", err);
    }).await.unwrap();

    //let bot = Bot {bot_id:336471730073632768, username: Some("test".to_string()) };
    let bot_id = "336471730073632768";
    let stream = format!("{}:{}", "channel:bot", bot_id);
    let consumer = format!("{}:{}", "comsumer:bot", bot_id);

    let result: Result<(), _> = connnection.xgroup_create_mkstream(stream, consumer, "$").await;
    match result {
        Err(err) => {
            println!("failed to execute with err={}", err);
        }
        Ok(_) => {
            println!("success to execute");
        }
    }

    println!("stoped")
}
