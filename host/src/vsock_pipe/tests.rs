use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

fn pipe(cap: usize) -> (PipeDevEnd, VsockStream, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let wake: WakeFn = {
        let count = count.clone();
        Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        })
    };
    let (dev, host) = VsockPipe::pair(cap, wake);
    (dev, host, count)
}

#[tokio::test]
async fn device_to_host_test() {
    let (mut dev, mut host, _) = pipe(64);
    dev.try_write(b"hello").unwrap();
    let mut buf = [0u8; 8];
    let n = host.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"hello");
}

#[tokio::test]
async fn host_to_device_test() {
    let (mut dev, mut host, _) = pipe(64);
    host.write_all(b"hello").await.unwrap();
    host.flush().await.unwrap();
    assert!(dev.is_readable());
    let mut buf = [0u8; 8];
    assert_eq!(dev.read_slice(&mut buf).unwrap(), 5);
    assert_eq!(&buf[..5], b"hello");
    assert!(!dev.is_readable());
}

#[tokio::test]
async fn directions_do_not_cross_test() {
    let (mut dev, mut host, _) = pipe(64);
    dev.try_write(b"from-guest").unwrap();
    host.write_all(b"from-host").await.unwrap();
    let mut buf = [0u8; 10];
    host.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"from-guest");
    let mut buf = [0u8; 9];
    assert_eq!(dev.read_slice(&mut buf).unwrap(), 9);
    assert_eq!(&buf, b"from-host");
}

#[tokio::test]
async fn backpressure_test() {
    let (mut dev, mut host, wakes) = pipe(4);
    assert_eq!(dev.try_write(b"abcd").unwrap(), 4);
    assert_eq!(
        dev.try_write(b"e").unwrap_err().kind(),
        ErrorKind::WouldBlock
    );
    let mut buf = [0u8; 2];
    let n = host.read(&mut buf).await.unwrap();
    assert_eq!((n, &buf[..n]), (2usize, &b"ab"[..]));
    assert!(wakes.load(Ordering::SeqCst) > 0);
    assert_eq!(dev.try_write(b"ef").unwrap(), 2);
}

#[tokio::test]
async fn host_backpressure_test() {
    let (mut dev, mut host, _) = pipe(4);
    host.write_all(b"abcd").await.unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let t = std::thread::spawn(move || {
        let mut buf = [0u8; 4];
        assert_eq!(dev.read_slice(&mut buf).unwrap(), 4);
        assert_eq!(&buf, b"abcd");
        done_rx.recv().unwrap();
        assert_eq!(dev.read_slice(&mut buf).unwrap(), 4);
        assert_eq!(&buf, b"efgh");
    });
    host.write_all(b"efgh").await.unwrap();
    done_tx.send(()).unwrap();
    t.join().unwrap();
}

#[tokio::test]
async fn host_shutdown_eof_test() {
    let (mut dev, mut host, _) = pipe(4);
    host.shutdown().await.unwrap();
    assert!(dev.is_readable());
    let mut buf = [0u8; 4];
    assert_eq!(dev.read_slice(&mut buf).unwrap(), 0);
}

#[tokio::test]
async fn host_drop_eof_test() {
    let (mut dev, host, wakes) = pipe(4);
    drop(host);
    assert!(wakes.load(Ordering::SeqCst) > 0);
    let mut buf = [0u8; 4];
    assert_eq!(dev.read_slice(&mut buf).unwrap(), 0);
    assert_eq!(
        dev.try_write(b"x").unwrap_err().kind(),
        ErrorKind::BrokenPipe
    );
}

#[tokio::test]
async fn device_drop_test() {
    let (dev, mut host, _) = pipe(4);
    drop(dev);
    let mut buf = [0u8; 4];
    assert_eq!(host.read(&mut buf).await.unwrap(), 0);
    assert_eq!(
        host.write(b"x").await.unwrap_err().kind(),
        ErrorKind::BrokenPipe
    );
}

#[tokio::test]
async fn async_read_wakes_on_device_write_test() {
    let (mut dev, mut host, _) = pipe(64);
    let t = {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            dev.try_write(b"hi").unwrap();
        })
    };
    let mut buf = [0u8; 2];
    let n = host.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"hi");
    t.join().unwrap();
}

#[tokio::test]
async fn large_transfer_test() {
    let (mut dev, mut host, _) = pipe(1 << 12);
    let payload: Vec<u8> = (0..(1u64 << 20)).map(|i| (i % 251) as u8).collect();
    let src = payload.clone();
    let pump = std::thread::spawn(move || {
        let mut pending = src;
        while !pending.is_empty() {
            match dev.try_write(&pending) {
                Ok(n) => {
                    if n == pending.len() {
                        pending.clear();
                    } else {
                        pending.drain(..n);
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_micros(100));
                }
                Err(e) => panic!("{e}"),
            }
        }
    });
    let mut got = Vec::new();
    let mut buf = [0u8; 1000];
    while got.len() < payload.len() {
        let n = host.read(&mut buf).await.unwrap();
        assert!(n > 0);
        got.extend_from_slice(&buf[..n]);
    }
    pump.join().unwrap();
    assert_eq!(got, payload);
}
