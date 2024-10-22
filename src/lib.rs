#![cfg_attr(docsrs, feature(doc_cfg))]

//! Rusty distributed locking backed by Redis.
//!
//! ```no_run
//! # use redis::AsyncCommands;
//! # #[allow(dependency_on_unit_never_type_fallback)]
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # tokio::runtime::Runtime::new()?.block_on(async {
//! # let client = todo!();
//! // Setup.
//! redis_lock::setup(&client).await?;
//! let mut lock = redis_lock::MultiResourceLock::new(client.clone())?;
//! let mut conn = client.get_multiplexed_async_connection().await?;
//! let from = "account1";
//! let to = "account2";
//! let resources = vec![String::from(from), String::from(to)];
//! // Acquire lock.
//! let opt = lock.lock_default(&resources).await?;
//! let guard = opt.ok_or("timed out")?;
//! // Perform transfer.
//! let amount = 500;
//! let from_balance: i64 = conn.get("account1").await?;
//! // Execute transaction if the sender has enough funds.
//! if from_balance >= amount {
//!     let to_balance: i64 = conn.get(to).await?;
//!     let new_from = from_balance.checked_sub(amount).ok_or("underflow")?;
//!     conn.set(from, new_from).await?;
//!     let new_to = to_balance.checked_add(amount).ok_or("overflow")?;
//!     conn.set(to, new_to).await?;
//! }
//! // Lock releases when dropped.
//! # Ok(())
//! # })
//! # }
//! ```
//!
//! ## Vs [rslock](https://github.com/hexcowboy/rslock)
//!
//! I would recommend this library over [rslock](https://github.com/hexcowboy/rslock) when:
//! - your application is focussed on `async`.
//! - your application does operations that require exclusive access to multiple resources.
//!
//! ## Similar work
//!
//! - <https://github.com/hexcowboy/rslock>

use redis::{Client, RedisResult};
use std::error::Error;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::task;
use uuid::Uuid;

/// Synchronous implementation of the lock.
#[cfg(feature = "sync")]
#[cfg_attr(docsrs, doc(cfg(feature = "sync")))]
pub mod sync;

/// A distributed mutual exclusion lock backed by Redis.
///
/// Supports exclusion based on multiple resources and partial overlaps.
///
/// E.g. a lock on resources `["a", "b"]` will block a lock on `["a"]` or `["b", "c"]`.
pub struct MultiResourceLock {
    /// The Redis client.
    client: Client,
}

impl std::fmt::Debug for MultiResourceLock {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiResourceLock")
            .field("conn", &"..")
            .finish()
    }
}

/// Initializes a Redis instance with the Lua library functions required for locking.
///
/// This only needs to be done once per Redis instance, although re-doing it should be fine.
///
/// # Errors
///
/// - When [`Client::get_connection`] errors.
/// - When the Lua library functions cannot be loaded into Redis.
#[inline]
pub async fn setup(client: &Client) -> Result<(), Box<dyn Error>> {
    // Connect to Redis
    let mut con = client.get_multiplexed_async_connection().await?;

    // Define your Lua library
    let lua_library = include_str!("functions.lua");

    // Load the Lua library into Redis
    redis::cmd("FUNCTION")
        .arg("LOAD")
        .arg("REPLACE")
        .arg(lua_library)
        .exec_async(&mut con)
        .await?;

    Ok(())
}

/// Default expiration duration for the lock.
pub const DEFAULT_EXPIRATION: Duration = Duration::from_secs(3600);
/// Default timeout duration for acquiring the lock.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// Default sleep duration between attempts to acquire the lock.
pub const DEFAULT_SLEEP: Duration = Duration::from_secs(1);

impl MultiResourceLock {
    /// Create a new instance of the lock.
    ///
    /// # Errors
    ///
    /// When [`Client::get_connection`] errors.
    #[inline]
    pub fn new(client: Client) -> RedisResult<Self> {
        Ok(MultiResourceLock { client })
    }

    /// Calls [`MultiResourceLock::acquire`] with [`DEFAULT_EXPIRATION`], [`DEFAULT_TIMEOUT`] and [`DEFAULT_SLEEP`].
    ///
    /// # Errors
    ///
    /// When [`MultiResourceLock::acquire`] errors.
    #[inline]
    pub async fn acquire_default(&mut self, resources: &[String]) -> RedisResult<Option<String>> {
        self.acquire(
            resources,
            DEFAULT_EXPIRATION,
            DEFAULT_TIMEOUT,
            DEFAULT_SLEEP,
        )
        .await
    }

    /// Attempts to acquire the lock blocking until the lock can be acquired.
    ///
    /// Blocks up to `timeout` duration making attempts every `sleep` duration.
    ///
    /// Returns `None` when it times out.
    ///
    /// # Errors
    ///
    /// When [`MultiResourceLock::try_acquire`] errors.
    #[inline]
    pub async fn acquire(
        &mut self,
        resources: &[String],
        expiration: Duration,
        timeout: Duration,
        sleep: Duration,
    ) -> RedisResult<Option<String>> {
        let now = std::time::Instant::now();
        loop {
            if now.elapsed() > timeout {
                return Ok(None);
            }
            match self.try_acquire(resources, expiration).await? {
                Some(res) => break Ok(Some(res)),
                None => tokio::time::sleep(sleep).await,
            }
        }
    }

    /// Calls [`MultiResourceLock::try_acquire`] with [`DEFAULT_EXPIRATION`].
    ///
    /// # Errors
    ///
    /// When [`MultiResourceLock::try_acquire`] errors.
    #[inline]
    pub async fn try_acquire_default(
        &mut self,
        resources: &[String],
    ) -> RedisResult<Option<String>> {
        self.try_acquire(resources, DEFAULT_EXPIRATION).await
    }

    /// Attempts to acquire the lock returning immediately if it cannot be immediately acquired.
    ///
    /// # Errors
    ///
    /// - When the `acquire_lock` function is missing from the Redis instance.
    #[inline]
    pub async fn try_acquire(
        &mut self,
        resources: &[String],
        expiration: Duration,
    ) -> RedisResult<Option<String>> {
        let mut connection = self.client.get_multiplexed_async_connection().await?;
        let lock_id = Uuid::new_v4().to_string();
        let mut args = vec![lock_id.clone(), expiration.as_millis().to_string()];
        args.extend(resources.iter().cloned());

        let result: Option<String> = redis::cmd("FCALL")
            .arg("acquire_lock")
            .arg(&args)
            .query_async(&mut connection)
            .await?;

        Ok(result)
    }

    /// Releases a held lock.
    ///
    /// # Errors
    ///
    /// - When the `release_lock` function is missing from the Redis instance.
    /// - When `lock_id` does not refer to a held lock.
    #[inline]
    pub async fn release(&mut self, lock_id: &str) -> RedisResult<usize> {
        let mut connection = self.client.get_multiplexed_async_connection().await?;
        let result: usize = redis::cmd("FCALL")
            .arg("release_lock")
            .arg(lock_id)
            .query_async(&mut connection)
            .await?;

        Ok(result)
    }

    /// Calls [`MultiResourceLock::try_lock`] with [`DEFAULT_EXPIRATION`].
    ///
    /// # Errors
    ///
    /// When [`MultiResourceLock::try_lock`] errors.
    #[inline]
    pub async fn try_lock_default(
        &mut self,
        resources: &[String],
    ) -> RedisResult<Option<MultiResourceGuard>> {
        self.try_lock(resources, DEFAULT_EXPIRATION).await
    }

    /// Attempts to acquire the lock returning immediately if it cannot be immediately acquired.
    ///
    /// Wraps the result in a guard that releases the lock when dropped.
    ///
    /// # Errors
    ///
    /// When [`MultiResourceLock::try_acquire`] errors.
    #[inline]
    pub async fn try_lock(
        &mut self,
        resources: &[String],
        expiration: Duration,
    ) -> RedisResult<Option<MultiResourceGuard<'_>>> {
        self.try_acquire(resources, expiration).await.map(|result| {
            result.map(|lock_id| MultiResourceGuard {
                lock: self,
                lock_id,
                rt: Handle::current(),
            })
        })
    }

    /// Calls [`MultiResourceLock::lock`] with [`DEFAULT_EXPIRATION`], [`DEFAULT_TIMEOUT`] and [`DEFAULT_SLEEP`].
    ///
    /// # Errors
    ///
    /// When [`MultiResourceLock::lock`] errors.
    #[inline]
    pub async fn lock_default(
        &mut self,
        resources: &[String],
    ) -> RedisResult<Option<MultiResourceGuard<'_>>> {
        self.lock(
            resources,
            DEFAULT_EXPIRATION,
            DEFAULT_TIMEOUT,
            DEFAULT_SLEEP,
        )
        .await
    }

    /// Attempts to acquire the lock blocking until the lock can be acquired.
    ///
    /// Blocks up to `timeout` duration making attempts every `sleep` duration.
    ///
    /// Returns `None` when it times out.
    ///
    /// Wraps the result in a guard that releases the lock when dropped.
    ///
    /// # Errors
    ///
    /// When [`MultiResourceLock::acquire`] errors.
    #[inline]
    pub async fn lock(
        &mut self,
        resources: &[String],
        expiration: Duration,
        timeout: Duration,
        sleep: Duration,
    ) -> RedisResult<Option<MultiResourceGuard<'_>>> {
        self.acquire(resources, expiration, timeout, sleep)
            .await
            .map(|result| {
                result.map(|lock_id| MultiResourceGuard {
                    lock: self,
                    lock_id,
                    rt: Handle::current(),
                })
            })
    }
}

/// A guard that releases the lock when it is dropped.
#[derive(Debug)]
pub struct MultiResourceGuard<'a> {
    /// The lock instance.
    lock: &'a mut MultiResourceLock,
    /// The lock identifier.
    lock_id: String,
    /// Handle to the tokio runtime.
    rt: Handle,
}

#[expect(
    clippy::unwrap_used,
    reason = "You can't propagate errors in a `Drop` implementation."
)]
impl Drop for MultiResourceGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        let mut lock = MultiResourceLock {
            client: self.lock.client.clone(),
        };
        let lock_id = self.lock_id.clone();
        let rt = self.rt.clone();
        task::spawn_blocking(move || {
            rt.block_on(async { lock.release(&lock_id).await }).unwrap();
        });
    }
}
