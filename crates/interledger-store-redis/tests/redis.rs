extern crate interledger_store_redis;
#[macro_use]
extern crate log;
#[macro_use]
extern crate lazy_static;

use bytes::Bytes;
use env_logger;
use futures::{future, Future};
use interledger_api::{AccountDetails, NodeStore};
use interledger_store_redis::{connect, connect_with_poll_interval, RedisStore};
use parking_lot::Mutex;
use redis;
use std::{
    process,
    thread::sleep,
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, timer::Delay};

lazy_static! {
    static ref ACCOUNT_DETAILS_0: AccountDetails = AccountDetails {
        ilp_address: b"example.alice".to_vec(),
        asset_scale: 6,
        asset_code: "XYZ".to_string(),
        max_packet_amount: 1000,
        http_endpoint: Some("http://example.com/ilp".to_string()),
        http_incoming_authorization: Some("Bearer incoming_auth_token".to_string()),
        http_outgoing_authorization: Some("outgoing_auth_token".to_string()),
        btp_uri: Some("btp+ws://example.com/btp".to_string()),
        btp_incoming_authorization: Some("btp_token".to_string()),
        is_admin: true,
        xrp_address: Some("rELhRfZ7YS31jbouULKYLB64KmrizFuC3T".to_string()),
        settle_threshold: Some(0),
        settle_to: Some(-1000),
    };
    static ref ACCOUNT_DETAILS_1: AccountDetails = AccountDetails {
        ilp_address: b"example.bob".to_vec(),
        asset_scale: 9,
        asset_code: "ABC".to_string(),
        max_packet_amount: 1_000_000,
        http_endpoint: Some("http://example.com/ilp".to_string()),
        http_incoming_authorization: Some("Basic QWxhZGRpbjpPcGVuU2VzYW1l".to_string()),
        http_outgoing_authorization: Some("outgoing_auth_token".to_string()),
        btp_uri: Some("btp+ws://example.com/btp".to_string()),
        btp_incoming_authorization: Some("other_btp_token".to_string()),
        is_admin: true,
        xrp_address: Some("rMLwdY4w8FT8zCEUL9q9173NrvpLGLEFDu".to_string()),
        settle_threshold: Some(0),
        settle_to: Some(-1000),
    };
    static ref TEST_MUTEX: Mutex<()> = Mutex::new(());
}

// Test helpers copied from https://github.com/mitsuhiko/redis-rs/blob/master/tests/support/mod.rs
pub struct RedisServer {
    process: process::Child,
    uri: String,
}

fn get_open_port() -> u16 {
    let listener = net2::TcpBuilder::new_v4()
        .unwrap()
        .reuse_address(true)
        .unwrap()
        .bind("127.0.0.1:0")
        .unwrap()
        .listen(1)
        .unwrap();
    listener.local_addr().unwrap().port()
}

impl RedisServer {
    pub fn new() -> RedisServer {
        let mut cmd = process::Command::new("redis-server");
        // Comment these lines out to see Redis log output
        cmd.stdout(process::Stdio::null())
            .stderr(process::Stdio::null());

        // this is technically a race but we can't do better with
        // the tools that redis gives us :(
        let port = get_open_port();
        cmd.arg("--loglevel").arg("verbose");
        cmd.arg("--port")
            .arg(port.to_string())
            .arg("--bind")
            .arg("127.0.0.1");
        let process = cmd.spawn().unwrap();

        debug!("Spawning redis server on port: {}", port);
        let mut server = RedisServer {
            process: process,
            uri: format!("redis://127.0.0.1:{}", port),
        };
        server.flush_db();

        server
    }

    pub fn redis_uri(&self) -> &str {
        self.uri.as_ref()
    }

    pub fn wait(&mut self) {
        self.process.wait().unwrap();
    }

    pub fn stop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }

    fn flush_db(&mut self) {
        let client = redis::Client::open(self.redis_uri()).unwrap();
        let con;

        let millisecond = Duration::from_millis(1);
        loop {
            match client.get_connection() {
                Err(err) => {
                    if err.is_connection_refusal() {
                        sleep(millisecond);
                    } else {
                        panic!("Could not connect: {}", err);
                    }
                }
                Ok(x) => {
                    con = x;
                    break;
                }
            }
        }
        redis::cmd("FLUSHDB").execute(&con);
        debug!("Flushed db");
    }
}

impl Default for RedisServer {
    fn default() -> Self {
        RedisServer::new()
    }
}

impl Drop for RedisServer {
    fn drop(&mut self) {
        self.stop()
    }
}

fn test_store() -> impl Future<Item = (RedisStore, RedisServer), Error = ()> {
    let server = RedisServer::default();
    connect(server.redis_uri()).and_then(|store| {
        let store_clone = store.clone();
        store
            .clone()
            .insert_account(ACCOUNT_DETAILS_0.clone())
            .and_then(move |_| store_clone.insert_account(ACCOUNT_DETAILS_1.clone()))
            .and_then(|_| Ok((store, server)))
    })
}

fn block_on<F>(f: F) -> Result<F::Item, F::Error>
where
    F: Future + Send + 'static,
    F::Item: Send,
    F::Error: Send,
{
    // Only run one test at a time
    let _ = env_logger::try_init();
    let lock = TEST_MUTEX.lock();
    let mut runtime = Runtime::new().unwrap();
    let result = runtime.block_on(f);
    drop(lock);
    result
}

mod connect_store {
    use super::*;

    #[test]
    fn fails_if_db_unavailable() {
        let mut runtime = Runtime::new().unwrap();
        runtime
            .block_on(future::lazy(
                || -> Box<Future<Item = (), Error = ()> + Send> {
                    Box::new(
                        connect(format!("redis://127.0.0.1:{}", get_open_port()).as_str()).then(
                            |result| {
                                assert!(result.is_err());
                                Ok(())
                            },
                        ),
                    )
                },
            ))
            .unwrap();
    }
}

mod insert_accounts {
    use super::*;

    #[test]
    fn insert_accounts() {
        block_on(test_store().and_then(|(_store, server)| {
            redis::Client::open(server.redis_uri())
                .unwrap()
                .get_async_connection()
                .map_err(|err| panic!(err))
                .and_then(move |client| {
                    redis::cmd("HGETALL")
                        .arg("accounts:0")
                        .query_async(client)
                        .and_then(move |(_connection, values): (_, redis::Value)| {
                            let _ = server;
                            if let redis::Value::Bulk(ref items) = values {
                                assert_eq!(items.len(), 14 * 2);
                                Ok(())
                            } else {
                                panic!("not bulk value");
                            }
                        })
                        .map_err(|_err| panic!("Error querying redis"))
                })
        }))
        .unwrap();
    }

    #[test]
    fn fails_on_duplicate_xrp_address() {
        let result = block_on(test_store().and_then(|(store, server)| {
            store
                .insert_account(AccountDetails {
                    ilp_address: b"example.charlie".to_vec(),
                    asset_scale: 6,
                    asset_code: "XYZ".to_string(),
                    max_packet_amount: 1000,
                    http_endpoint: None,
                    http_incoming_authorization: None,
                    http_outgoing_authorization: None,
                    btp_uri: None,
                    btp_incoming_authorization: None,
                    is_admin: false,
                    xrp_address: Some("rELhRfZ7YS31jbouULKYLB64KmrizFuC3T".to_string()),
                    settle_threshold: Some(0),
                    settle_to: Some(-1000),
                })
                .then(move |result| {
                    let _ = server;
                    result
                })
        }));
        assert!(result.is_err());
    }

    #[test]
    fn fails_on_duplicate_http_incoming_auth() {
        let result = block_on(test_store().and_then(|(store, server)| {
            store
                .insert_account(AccountDetails {
                    ilp_address: b"example.charlie".to_vec(),
                    asset_scale: 6,
                    asset_code: "XYZ".to_string(),
                    max_packet_amount: 1000,
                    http_endpoint: None,
                    http_incoming_authorization: Some("Bearer incoming_auth_token".to_string()),
                    http_outgoing_authorization: None,
                    btp_uri: None,
                    btp_incoming_authorization: None,
                    is_admin: false,
                    xrp_address: None,
                    settle_threshold: None,
                    settle_to: None,
                })
                .then(move |result| {
                    let _ = server;
                    result
                })
        }));
        assert!(result.is_err());
    }

    #[test]
    fn fails_on_duplicate_btp_incoming_auth() {
        let result = block_on(test_store().and_then(|(store, server)| {
            store
                .insert_account(AccountDetails {
                    ilp_address: b"example.charlie".to_vec(),
                    asset_scale: 6,
                    asset_code: "XYZ".to_string(),
                    max_packet_amount: 1000,
                    http_endpoint: None,
                    http_incoming_authorization: None,
                    http_outgoing_authorization: None,
                    btp_uri: None,
                    btp_incoming_authorization: Some("btp_token".to_string()),
                    is_admin: false,
                    xrp_address: None,
                    settle_threshold: None,
                    settle_to: None,
                })
                .then(move |result| {
                    let _ = server;
                    result
                })
        }));
        assert!(result.is_err());
    }
}

mod node_store {
    use super::*;
    use interledger_api::NodeStore;
    use interledger_service_util::ExchangeRateStore;

    #[test]
    fn get_all_accounts() {
        block_on(test_store().and_then(|(store, server)| {
            store.get_all_accounts().and_then(move |accounts| {
                assert_eq!(accounts.len(), 2);
                let _ = server;
                Ok(())
            })
        }))
        .unwrap();
    }

    #[test]
    fn set_rates() {
        block_on(test_store().and_then(|(store, server)| {
            let store_clone = store.clone();
            let rates = store.get_exchange_rates(&["ABC", "XYZ"]);
            assert!(rates.is_err());
            store
                .set_rates(vec![("ABC".to_string(), 500.0), ("XYZ".to_string(), 0.005)])
                .and_then(move |_| {
                    let rates = store_clone.get_exchange_rates(&["XYZ", "ABC"]).unwrap();
                    assert_eq!(rates[0].to_string(), "0.005");
                    assert_eq!(rates[1].to_string(), "500");
                    let _ = server;
                    Ok(())
                })
        }))
        .unwrap();
    }
}

mod get_accounts {
    use super::*;
    use interledger_ildcp::IldcpAccount;
    use interledger_service::AccountStore;

    #[test]
    fn gets_single_account() {
        block_on(test_store().and_then(|(store, server)| {
            store.get_accounts(vec![1]).and_then(move |accounts| {
                assert_eq!(accounts[0].client_address(), b"example.bob");
                let _ = server;
                Ok(())
            })
        }))
        .unwrap();
    }

    #[test]
    fn gets_multiple() {
        block_on(test_store().and_then(|(store, server)| {
            store.get_accounts(vec![1, 0]).and_then(move |accounts| {
                // note reverse order is intentional
                assert_eq!(accounts[0].client_address(), b"example.bob");
                assert_eq!(accounts[1].client_address(), b"example.alice");
                let _server = server;
                Ok(())
            })
        }))
        .unwrap();
    }

    #[test]
    fn errors_for_unknown_accounts() {
        let result = block_on(test_store().and_then(|(store, server)| {
            store.get_accounts(vec![0, 2]).then(move |result| {
                let _ = server;
                result
            })
        }));
        assert!(result.is_err());
    }
}

mod routes_and_rates {
    use super::*;
    use interledger_router::RouterStore;
    use interledger_service_util::ExchangeRateStore;

    #[test]
    fn polls_for_route_updates() {
        let server = RedisServer::new();
        block_on(
            connect_with_poll_interval(server.redis_uri(), 1).and_then(|store| {
                assert_eq!(store.routing_table().len(), 0);
                let store_clone_1 = store.clone();
                let store_clone_2 = store.clone();
                store
                    .clone()
                    .insert_account(ACCOUNT_DETAILS_0.clone())
                    .and_then(move |_| {
                        let routing_table = store_clone_1.routing_table();
                        assert_eq!(routing_table.len(), 1);
                        assert_eq!(
                            *routing_table.get(&Bytes::from("example.alice")).unwrap(),
                            0
                        );
                        store_clone_1.insert_account(AccountDetails {
                            ilp_address: b"example.bob".to_vec(),
                            asset_scale: 6,
                            asset_code: "XYZ".to_string(),
                            max_packet_amount: 1000,
                            http_endpoint: None,
                            http_incoming_authorization: None,
                            http_outgoing_authorization: None,
                            btp_uri: None,
                            btp_incoming_authorization: None,
                            is_admin: false,
                            xrp_address: None,
                            settle_threshold: None,
                            settle_to: None,
                        })
                    })
                    .and_then(move |_| {
                        let routing_table = store_clone_2.routing_table();
                        assert_eq!(routing_table.len(), 2);
                        assert_eq!(*routing_table.get(&Bytes::from("example.bob")).unwrap(), 1);
                        redis::Client::open(server.redis_uri())
                            .unwrap()
                            .get_async_connection()
                            .map_err(|_| panic!("Unable to get client connection to db"))
                            .and_then(|client| {
                                redis::cmd("HMSET")
                                    .arg("routes")
                                    .arg("example.alice")
                                    .arg(1)
                                    .arg("example.charlie")
                                    .arg(0)
                                    .query_async(client)
                                    .and_then(|(_connection, _result): (_, redis::Value)| Ok(()))
                                    .map_err(|err| panic!(err))
                            })
                            .and_then(|_| {
                                Delay::new(Instant::now() + Duration::from_millis(10))
                                    .then(|_| Ok(()))
                            })
                            .and_then(move |_| {
                                let routing_table = store_clone_2.routing_table();
                                assert_eq!(routing_table.len(), 3);
                                assert_eq!(
                                    *routing_table.get(&Bytes::from("example.alice")).unwrap(),
                                    1
                                );
                                assert_eq!(
                                    *routing_table.get(&Bytes::from("example.bob")).unwrap(),
                                    1
                                );
                                assert_eq!(
                                    *routing_table.get(&Bytes::from("example.charlie")).unwrap(),
                                    0
                                );
                                assert!(routing_table.get(&Bytes::from("example.other")).is_none());
                                let _server = server;
                                Ok(())
                            })
                    })
            }),
        )
        .unwrap();
    }

    #[test]
    fn polls_for_rate_updates() {
        let server = RedisServer::new();
        block_on(
            connect_with_poll_interval(server.redis_uri(), 1).and_then(|store| {
                assert!(store.get_exchange_rates(&["ABC", "XYZ"]).is_err());
                store
                    .clone()
                    .set_rates(vec![
                        ("ABC".to_string(), 0.5f64),
                        ("DEF".to_string(), 9_999_999_999.0f64),
                    ])
                    .and_then(|_| {
                        Delay::new(Instant::now() + Duration::from_millis(10)).then(|_| Ok(()))
                    })
                    .and_then(move |_| {
                        assert_eq!(store.get_exchange_rates(&["ABC"]).unwrap(), vec![0.5]);
                        assert_eq!(
                            store.get_exchange_rates(&["ABC", "DEF"]).unwrap(),
                            vec![0.5, 9_999_999_999.0]
                        );
                        assert!(store.get_exchange_rates(&["ABC", "XYZ"]).is_err());
                        let _ = server;
                        Ok(())
                    })
            }),
        )
        .unwrap();
    }
}

mod balances {
    use super::*;
    use interledger_service::AccountStore;
    use interledger_service_util::BalanceStore;

    #[test]
    fn updating_and_rolling_back() {
        block_on(test_store().and_then(|(store, server)| {
            let store_clone_1 = store.clone();
            let store_clone_2 = store.clone();
            store
                .clone()
                .get_accounts(vec![0, 1])
                .map_err(|_err| panic!("Unable to get accounts"))
                .and_then(move |accounts| {
                    let account0 = accounts[0].clone();
                    let account1 = accounts[1].clone();
                    store
                        .update_balances(accounts[0].clone(), 100, accounts[1].clone(), 500)
                        .and_then(move |_| {
                            store_clone_1
                                .clone()
                                .get_balance(accounts[0].clone())
                                .join(store_clone_1.clone().get_balance(accounts[1].clone()))
                                .and_then(|(balance0, balance1)| {
                                    assert_eq!(balance0, 100);
                                    assert_eq!(balance1, -500);
                                    Ok(())
                                })
                        })
                        .and_then(move |_| {
                            store_clone_2
                                .clone()
                                .undo_balance_update(account0.clone(), 100, account1.clone(), 500)
                                .and_then(move |_| {
                                    store_clone_2
                                        .clone()
                                        .get_balance(account0.clone())
                                        .join(store_clone_2.clone().get_balance(account1.clone()))
                                        .and_then(|(balance0, balance1)| {
                                            assert_eq!(balance0, 0);
                                            assert_eq!(balance1, 0);
                                            let _server = server;
                                            Ok(())
                                        })
                                })
                        })
                })
        }))
        .unwrap();
    }
}

mod from_btp {
    use super::*;
    use interledger_btp::BtpStore;
    use interledger_service::Account;

    #[test]
    fn gets_account_from_btp_token() {
        block_on(test_store().and_then(|(store, server)| {
            store
                .get_account_from_btp_token("other_btp_token")
                .and_then(move |account| {
                    assert_eq!(account.id(), 1);
                    let _ = server;
                    Ok(())
                })
        }))
        .unwrap()
    }

    #[test]
    fn errors_on_unknown_btp_token() {
        let result = block_on(test_store().and_then(|(store, server)| {
            store
                .get_account_from_btp_token("unknown_btp_token")
                .then(move |result| {
                    let _ = server;
                    result
                })
        }));
        assert!(result.is_err());
    }
}

mod from_http {
    use super::*;
    use interledger_http::HttpStore;
    use interledger_service::Account;

    #[test]
    fn gets_account_from_http_bearer_token() {
        block_on(test_store().and_then(|(store, server)| {
            store
                .get_account_from_http_auth("Bearer incoming_auth_token")
                .and_then(move |account| {
                    assert_eq!(account.id(), 0);
                    let _ = server;
                    Ok(())
                })
        }))
        .unwrap()
    }

    #[test]
    fn gets_account_from_http_basic_auth() {
        block_on(test_store().and_then(|(store, server)| {
            store
                .get_account_from_http_auth("Basic QWxhZGRpbjpPcGVuU2VzYW1l")
                .and_then(move |account| {
                    assert_eq!(account.id(), 1);
                    let _ = server;
                    Ok(())
                })
        }))
        .unwrap()
    }

    #[test]
    fn errors_on_unknown_http_auth() {
        let result = block_on(test_store().and_then(|(store, server)| {
            store
                .get_account_from_http_auth("Bearer unknown_token")
                .then(move |result| {
                    let _ = server;
                    result
                })
        }));
        assert!(result.is_err());
    }
}
