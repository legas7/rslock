use std::io;
use std::time::{Duration, Instant};

use futures::future::join_all;
use futures::Future;
use rand::{thread_rng, Rng, RngCore};
use redis::Value::Okay;
use redis::{Client, IntoConnectionInfo, RedisResult, Value};

const DEFAULT_RETRY_COUNT: u32 = 3;
const DEFAULT_RETRY_DELAY: Duration = Duration::from_millis(200);
const CLOCK_DRIFT_FACTOR: f32 = 0.01;
const UNLOCK_SCRIPT: &str = r#"
if redis.call("GET", KEYS[1]) == ARGV[1] then
  return redis.call("DEL", KEYS[1])
else
  return 0
end
"#;
const EXTEND_SCRIPT: &str = r#"
if redis.call("get", KEYS[1]) ~= ARGV[1] then
  return 0
else
  if redis.call("set", KEYS[1], ARGV[1], "PX", ARGV[2]) ~= nil then
    return 1
  else
    return 0
  end
end
"#;

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("Redis error: {0}")]
    Redis(#[from] redis::RedisError),

    #[error("Resource is unavailable")]
    Unavailable,

    #[error("TTL exceeded")]
    TtlExceeded,

    #[error("TTL too large")]
    TtlTooLarge,
}

/// The lock manager.
///
/// Implements the necessary functionality to acquire and release locks
/// and handles the Redis connections.
#[derive(Debug, Clone)]
pub struct LockManager {
    /// List of all Redis clients
    pub servers: Vec<Client>,
    quorum: u32,
    retry_count: u32,
    retry_delay: Duration,
}

#[derive(Debug, Clone)]
pub struct Lock<'a> {
    /// The resource to lock. Will be used as the key in Redis.
    pub resource: Vec<u8>,
    /// The value for this lock.
    pub val: Vec<u8>,
    /// Time the lock is still valid.
    /// Should only be slightly smaller than the requested TTL.
    pub validity_time: usize,
    /// Used to limit the lifetime of a lock to its lock manager.
    pub lock_manager: &'a LockManager,
}

/// Upon dropping the guard, `LockManager::unlock` will be ran synchronously on the executor.
///
/// This is known to block the tokio runtime if this happens inside of the context of a tokio runtime
/// if `tokio-comp` is enabled as a feature on this crate or the `redis` crate.
///
/// To eliminate this risk, if the `tokio-comp` flag is enabled, the `Drop` impl will not be compiled,
/// meaning that dropping the `LockGuard` will be a no-op.
/// Under this circumstance, `LockManager::unlock` can be called manually using the inner `lock` at the appropriate
/// point to release the lock taken in `Redis`.
#[derive(Debug, Clone)]
pub struct LockGuard<'a> {
    pub lock: Lock<'a>,
}

/// Dropping this guard inside the context of a tokio runtime if `tokio-comp` is enabled
/// will block the tokio runtime.
/// Because of this, the guard is not compiled if `tokio-comp` is enabled.
#[cfg(not(feature = "tokio-comp"))]
impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        futures::executor::block_on(self.lock.lock_manager.unlock(&self.lock));
    }
}

impl LockManager {
    /// Create a new lock manager instance, defined by the given Redis connection uris.
    /// Quorum is defined to be N/2+1, with N being the number of given Redis instances.
    ///
    /// Sample URI: `"redis://127.0.0.1:6379"`
    pub fn new<T: IntoConnectionInfo>(uris: Vec<T>) -> LockManager {
        let quorum = (uris.len() as u32) / 2 + 1;

        let servers: Vec<Client> = uris
            .into_iter()
            .map(|uri| Client::open(uri).unwrap())
            .collect();

        LockManager {
            servers,
            quorum,
            retry_count: DEFAULT_RETRY_COUNT,
            retry_delay: DEFAULT_RETRY_DELAY,
        }
    }

    /// Get 20 random bytes from the pseudorandom interface.
    pub fn get_unique_lock_id(&self) -> io::Result<Vec<u8>> {
        {
            let mut buf = [0u8; 20];
            thread_rng().fill_bytes(&mut buf);
            Ok(buf.to_vec())
        }
    }

    /// Set retry count and retry delay.
    ///
    /// Retry count defaults to `3`.
    /// Retry delay defaults to `200`.
    pub fn set_retry(&mut self, count: u32, delay: Duration) {
        self.retry_count = count;
        self.retry_delay = delay;
    }

    async fn lock_instance(
        client: &redis::Client,
        resource: &[u8],
        val: Vec<u8>,
        ttl: usize,
    ) -> bool {
        let mut con = match client.get_async_connection().await {
            Err(_) => return false,
            Ok(val) => val,
        };
        let result: RedisResult<Value> = redis::cmd("SET")
            .arg(resource)
            .arg(val)
            .arg("NX")
            .arg("PX")
            .arg(ttl)
            .query_async(&mut con)
            .await;

        match result {
            Ok(Okay) => true,
            Ok(_) | Err(_) => false,
        }
    }

    async fn extend_lock_instance(
        client: &redis::Client,
        resource: &[u8],
        val: &[u8],
        ttl: usize,
    ) -> bool {
        let mut con = match client.get_async_connection().await {
            Err(_) => return false,
            Ok(val) => val,
        };
        let script = redis::Script::new(EXTEND_SCRIPT);
        let result: RedisResult<i32> = script
            .key(resource)
            .arg(val)
            .arg(ttl)
            .invoke_async(&mut con)
            .await;
        match result {
            Ok(val) => val == 1,
            Err(_) => false,
        }
    }

    async fn unlock_instance(client: &redis::Client, resource: &[u8], val: &[u8]) -> bool {
        let mut con = match client.get_async_connection().await {
            Err(_) => return false,
            Ok(val) => val,
        };
        let script = redis::Script::new(UNLOCK_SCRIPT);
        let result: RedisResult<i32> = script.key(resource).arg(val).invoke_async(&mut con).await;
        match result {
            Ok(val) => val == 1,
            Err(_) => false,
        }
    }

    // Can be used for creating or extending a lock
    async fn exec_or_retry<'a, T, Fut>(
        &'a self,
        resource: &[u8],
        value: &[u8],
        ttl: usize,
        lock: T,
    ) -> Result<Lock<'a>, LockError>
    where
        T: Fn(&'a Client) -> Fut,
        Fut: Future<Output = bool>,
    {
        for _ in 0..self.retry_count {
            let start_time = Instant::now();
            let n = join_all(self.servers.iter().map(&lock))
                .await
                .into_iter()
                .fold(0, |count, locked| if locked { count + 1 } else { count });

            let drift = (ttl as f32 * CLOCK_DRIFT_FACTOR) as usize + 2;
            let elapsed = start_time.elapsed();
            let elapsed_ms =
                elapsed.as_secs() as usize * 1000 + elapsed.subsec_nanos() as usize / 1_000_000;
            if ttl <= drift + elapsed_ms {
                return Err(LockError::TtlExceeded);
            }
            let validity_time = ttl
                - drift
                - elapsed.as_secs() as usize * 1000
                - elapsed.subsec_nanos() as usize / 1_000_000;

            if n >= self.quorum && validity_time > 0 {
                return Ok(Lock {
                    lock_manager: self,
                    resource: resource.to_vec(),
                    val: value.to_vec(),
                    validity_time,
                });
            } else {
                join_all(
                    self.servers
                        .iter()
                        .map(|client| Self::unlock_instance(client, resource, value)),
                )
                .await;
            }

            let retry_delay: u64 = self
                .retry_delay
                .as_millis()
                .try_into()
                .map_err(|_| LockError::TtlTooLarge)?;
            let n = thread_rng().gen_range(0..retry_delay);
            tokio::time::sleep(Duration::from_millis(n)).await
        }

        Err(LockError::Unavailable)
    }

    /// Unlock the given lock.
    ///
    /// Unlock is best effort. It will simply try to contact all instances
    /// and remove the key.
    pub async fn unlock(&self, lock: &Lock<'_>) {
        join_all(
            self.servers
                .iter()
                .map(|client| Self::unlock_instance(client, &lock.resource, &lock.val)),
        )
        .await;
    }

    /// Acquire the lock for the given resource and the requested TTL.
    ///
    /// If it succeeds, a `Lock` instance is returned,
    /// including the value and the validity time
    ///
    /// If it fails. `None` is returned.
    /// A user should retry after a short wait time.
    ///
    /// May return `LockError::TtlTooLarge` if `ttl` is too large.
    pub async fn lock<'a>(&'a self, resource: &[u8], ttl: Duration) -> Result<Lock<'a>, LockError> {
        let val = self.get_unique_lock_id().map_err(LockError::Io)?;
        let ttl = ttl
            .as_millis()
            .try_into()
            .map_err(|_| LockError::TtlTooLarge)?;

        self.exec_or_retry(resource, &val.clone(), ttl, move |client| {
            Self::lock_instance(client, resource, val.clone(), ttl)
        })
        .await
    }

    /// Loops until the lock is acquired.
    ///
    /// The lock is placed in a guard that will unlock the lock when the guard is dropped.
    ///
    /// May return `LockError::TtlTooLarge` if `ttl` is too large.
    #[cfg(feature = "async-std-comp")]
    pub async fn acquire<'a>(
        &'a self,
        resource: &[u8],
        ttl: Duration,
    ) -> Result<LockGuard<'a>, LockError> {
        let lock = self.acquire_no_guard(resource, ttl).await?;
        Ok(LockGuard { lock })
    }

    /// Loops until the lock is acquired.
    ///
    /// Either lock's value must expire after the ttl has elapsed,
    /// or `LockManager::unlock` must be called to allow other clients to lock the same resource.
    ///
    /// May return `LockError::TtlTooLarge` if `ttl` is too large.
    pub async fn acquire_no_guard<'a>(
        &'a self,
        resource: &[u8],
        ttl: Duration,
    ) -> Result<Lock<'a>, LockError> {
        loop {
            match self.lock(resource, ttl).await {
                Ok(lock) => return Ok(lock),
                Err(LockError::TtlTooLarge) => return Err(LockError::TtlTooLarge),
                Err(_) => continue,
            }
        }
    }

    /// Extend the given lock by given time in milliseconds
    pub async fn extend<'a>(
        &'a self,
        lock: &Lock<'a>,
        ttl: Duration,
    ) -> Result<Lock<'a>, LockError> {
        let ttl = ttl
            .as_millis()
            .try_into()
            .map_err(|_| LockError::TtlTooLarge)?;

        self.exec_or_retry(&lock.resource, &lock.val, ttl, move |client| {
            Self::extend_lock_instance(client, &lock.resource, &lock.val, ttl)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use once_cell::sync::Lazy;
    use testcontainers::clients::Cli;
    use testcontainers::images::redis::Redis;
    use testcontainers::{Container, RunnableImage};

    use super::*;

    type Containers = Vec<Container<'static, Redis>>;

    static DOCKER: Lazy<Cli> = Lazy::new(Cli::docker);

    fn is_normal<T: Sized + Send + Sync + Unpin>() {}

    fn create_clients() -> (Containers, Vec<String>) {
        let containers: Containers = (1..=3)
            .map(|_| {
                let image = RunnableImage::from(Redis).with_tag("7-alpine");
                DOCKER.run(image)
            })
            .collect();

        let addresses = containers
            .iter()
            .map(|node| format!("redis://localhost:{}", node.get_host_port_ipv4(6379)))
            .collect();

        (containers, addresses)
    }

    // Test that the LockManager is Send + Sync
    #[test]
    fn test_is_normal() {
        is_normal::<LockManager>();
        is_normal::<LockError>();
        is_normal::<Lock>();
        is_normal::<LockGuard>();
    }

    #[tokio::test]
    async fn test_lock_get_unique_id() -> Result<()> {
        let rl = LockManager::new(Vec::<String>::new());
        assert_eq!(rl.get_unique_lock_id()?.len(), 20);

        Ok(())
    }

    #[tokio::test]
    async fn test_lock_get_unique_id_uniqueness() -> Result<()> {
        let rl = LockManager::new(Vec::<String>::new());

        let id1 = rl.get_unique_lock_id()?;
        let id2 = rl.get_unique_lock_id()?;

        assert_eq!(20, id1.len());
        assert_eq!(20, id2.len());
        assert_ne!(id1, id2);

        Ok(())
    }

    #[tokio::test]
    async fn test_lock_valid_instance() {
        let (_containers, addresses) = create_clients();

        let rl = LockManager::new(addresses.clone());

        assert_eq!(3, rl.servers.len());
        assert_eq!(2, rl.quorum);
    }

    #[tokio::test]
    async fn test_lock_direct_unlock_fails() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl = LockManager::new(addresses.clone());
        let key = rl.get_unique_lock_id()?;

        let val = rl.get_unique_lock_id()?;
        assert!(!LockManager::unlock_instance(&rl.servers[0], &key, &val).await);

        Ok(())
    }

    #[tokio::test]
    async fn test_lock_direct_unlock_succeeds() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl = LockManager::new(addresses.clone());
        let key = rl.get_unique_lock_id()?;

        let val = rl.get_unique_lock_id()?;
        let mut con = rl.servers[0].get_connection()?;
        redis::cmd("SET").arg(&*key).arg(&*val).execute(&mut con);

        assert!(LockManager::unlock_instance(&rl.servers[0], &key, &val).await);

        Ok(())
    }

    #[tokio::test]
    async fn test_lock_direct_lock_succeeds() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl = LockManager::new(addresses.clone());
        let key = rl.get_unique_lock_id()?;

        let val = rl.get_unique_lock_id()?;
        let mut con = rl.servers[0].get_connection()?;

        redis::cmd("DEL").arg(&*key).execute(&mut con);
        assert!(LockManager::lock_instance(&rl.servers[0], &key, val.clone(), 1000).await);

        Ok(())
    }

    #[tokio::test]
    async fn test_lock_unlock() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl = LockManager::new(addresses.clone());
        let key = rl.get_unique_lock_id()?;

        let val = rl.get_unique_lock_id()?;
        let mut con = rl.servers[0].get_connection()?;
        let _: () = redis::cmd("SET")
            .arg(&*key)
            .arg(&*val)
            .query(&mut con)
            .unwrap();

        let lock = Lock {
            lock_manager: &rl,
            resource: key,
            val,
            validity_time: 0,
        };

        rl.unlock(&lock).await;

        Ok(())
    }

    #[tokio::test]
    async fn test_lock_lock() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl = LockManager::new(addresses.clone());

        let key = rl.get_unique_lock_id()?;
        match rl.lock(&key, Duration::from_millis(1000)).await {
            Ok(lock) => {
                assert_eq!(key, lock.resource);
                assert_eq!(20, lock.val.len());
                assert!(lock.validity_time > 900);
                assert!(
                    lock.validity_time > 900,
                    "validity time: {}",
                    lock.validity_time
                );
            }
            Err(e) => panic!("{:?}", e),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_lock_lock_unlock() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl = LockManager::new(addresses.clone());
        let rl2 = LockManager::new(addresses.clone());

        let key = rl.get_unique_lock_id()?;

        let lock = rl.lock(&key, Duration::from_millis(1000)).await.unwrap();
        assert!(
            lock.validity_time > 900,
            "validity time: {}",
            lock.validity_time
        );

        if let Ok(_l) = rl2.lock(&key, Duration::from_millis(1000)).await {
            panic!("Lock acquired, even though it should be locked")
        }

        rl.unlock(&lock).await;

        match rl2.lock(&key, Duration::from_millis(1000)).await {
            Ok(l) => assert!(l.validity_time > 900),
            Err(_) => panic!("Lock couldn't be acquired"),
        }

        Ok(())
    }

    #[cfg(all(not(feature = "tokio-comp"), feature = "async-std-comp"))]
    #[tokio::test]
    async fn test_lock_lock_unlock_raii() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl = LockManager::new(addresses.clone());
        let rl2 = LockManager::new(addresses.clone());
        let key = rl.get_unique_lock_id()?;

        async {
            let lock_guard = rl.acquire(&key, Duration::from_millis(1000)).await.unwrap();
            let lock = &lock_guard.lock;
            assert!(
                lock.validity_time > 900,
                "validity time: {}",
                lock.validity_time
            );

            if let Ok(_l) = rl2.lock(&key, Duration::from_millis(1000)).await {
                panic!("Lock acquired, even though it should be locked")
            }
        }
        .await;

        match rl2.lock(&key, Duration::from_millis(1000)).await {
            Ok(l) => assert!(l.validity_time > 900),
            Err(_) => panic!("Lock couldn't be acquired"),
        }

        Ok(())
    }

    #[cfg(feature = "tokio-comp")]
    #[tokio::test]
    async fn test_lock_raii_does_not_unlock_with_tokio_enabled() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl1 = LockManager::new(addresses.clone());
        let rl2 = LockManager::new(addresses.clone());
        let key = rl1.get_unique_lock_id()?;

        async {
            let lock_guard = rl1
                .acquire(&key, Duration::from_millis(10_000))
                .await
                .expect("LockManage rl1 should be able to acquire lock");
            let lock = &lock_guard.lock;
            assert!(
                lock.validity_time > 0,
                "validity time: {}",
                lock.validity_time
            );

            // Acquire lock2 and assert it can't be acquired
            if let Ok(_l) = rl2.lock(&key, Duration::from_millis(1000)).await {
                panic!("Lock acquired, even though it should be locked")
            }
        }
        .await;

        if let Ok(_) = rl2.lock(&key, Duration::from_millis(1000)).await {
            panic!("Lock couldn't be acquired");
        }

        Ok(())
    }

    #[cfg(feature = "async-std-comp")]
    #[tokio::test]
    async fn test_lock_extend_lock() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl1 = LockManager::new(addresses.clone());
        let rl2 = LockManager::new(addresses.clone());

        let key = rl1.get_unique_lock_id()?;

        async {
            let lock1 = rl1
                .acquire(&key, Duration::from_millis(1000))
                .await
                .unwrap();

            // Wait half a second before locking again
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

            rl1.extend(&lock1.lock, Duration::from_millis(1000))
                .await
                .unwrap();

            // Wait another half a second to see if lock2 can unlock
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

            // Assert lock2 can't access after extended lock
            match rl2.lock(&key, Duration::from_millis(1000)).await {
                Ok(_) => panic!("Expected an error when extending the lock but didn't receive one"),
                Err(e) => match e {
                    LockError::Unavailable => (),
                    _ => panic!("Unexpected error when extending lock"),
                },
            }
        }
        .await;

        Ok(())
    }

    #[cfg(feature = "async-std-comp")]
    #[tokio::test]
    async fn test_lock_extend_lock_releases() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl1 = LockManager::new(addresses.clone());
        let rl2 = LockManager::new(addresses.clone());

        let key = rl1.get_unique_lock_id()?;

        async {
            // Create 500ms lock and immediately extend 500ms
            let lock1 = rl1.acquire(&key, Duration::from_millis(500)).await.unwrap();
            rl1.extend(&lock1.lock, Duration::from_millis(500))
                .await
                .unwrap();

            // Wait one second for the lock to expire
            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

            // Assert rl2 can lock with the key now
            if rl2.lock(&key, Duration::from_millis(1000)).await.is_err() {
                panic!("Unexpected error when trying to claim free lock after extend expired")
            }

            // Also assert rl1 can't reuse lock1
            match rl1.extend(&lock1.lock, Duration::from_millis(1000)).await {
                Ok(_) => panic!("Did not expect OK() when re-extending rl1"),
                Err(e) => match e {
                    LockError::Unavailable => (),
                    _ => panic!("Expected lockError::Unavailable when re-extending rl1"),
                },
            }
        }
        .await;

        Ok(())
    }

    #[tokio::test]
    async fn test_lock_with_short_ttl_and_retries() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let mut rl = LockManager::new(addresses.clone());
        // Set a high retry count to ensure retries happen
        rl.set_retry(10, Duration::from_millis(10)); // Retry 10 times with 10 milliseconds delay

        let key = rl.get_unique_lock_id()?;

        // Use a very short TTL
        let ttl = Duration::from_millis(1);

        // Acquire lock
        let lock_result = rl.lock(&key, ttl).await;

        // Check if the error returned is TtlExceeded
        match lock_result {
            Err(LockError::TtlExceeded) => (), // Test passes
            _ => panic!("Expected LockError::TtlExceeded, but got {:?}", lock_result),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_lock_ttl_duration_conversion_error() {
        let (_containers, addresses) = create_clients();
        let rl = LockManager::new(addresses.clone());
        let key = rl.get_unique_lock_id().unwrap();

        // Too big Duration, fails - technical limit is from_millis(u64::MAX)
        let ttl = Duration::from_secs(u64::MAX);
        if rl.lock(&key, ttl).await.is_ok() {
            panic!("Expected LockError::TtlTooLarge")
        }
    }
}
