//! Benchmark implementation details of the threaded scheduler. These benches are
//! intended to be used as a form of regression testing and not as a general
//! purpose benchmark demonstrating real-world performance.

use tokio::runtime::{self, Runtime};

use criterion::{black_box, criterion_group, criterion_main, Criterion};

const NUM_SPAWN: usize = 1_000;
const NUM_YIELD: usize = 10_000;

fn rt_curr_spawn_many_local(c: &mut Criterion) {
    let rt = rt();
    let mut handles = Vec::with_capacity(NUM_SPAWN);

    c.bench_function("spawn_many_local", |b| {
        b.iter(|| {
            rt.block_on(async {
                for _ in 0..NUM_SPAWN {
                    handles.push(tokio::spawn(async move {}));
                }

                for handle in handles.drain(..) {
                    handle.await.unwrap();
                }
            });
        })
    });
}

fn rt_curr_spawn_many_remote_idle(c: &mut Criterion) {
    let rt = rt();
    let rt_handle = rt.handle();
    let mut handles = Vec::with_capacity(NUM_SPAWN);

    c.bench_function("spawn_many_remote_idle", |b| {
        b.iter(|| {
            for _ in 0..NUM_SPAWN {
                handles.push(rt_handle.spawn(async {}));
            }

            rt.block_on(async {
                for handle in handles.drain(..) {
                    handle.await.unwrap();
                }
            });
        })
    });
}

fn rt_curr_spawn_many_remote_busy(c: &mut Criterion) {
    let rt = rt();
    let rt_handle = rt.handle();
    let mut handles = Vec::with_capacity(NUM_SPAWN);

    rt.spawn(async {
        fn iter() {
            tokio::spawn(async { iter() });
        }

        iter()
    });

    c.bench_function("spawn_many_remote_busy", |b| {
        b.iter(|| {
            for _ in 0..NUM_SPAWN {
                handles.push(rt_handle.spawn(async {}));
            }

            rt.block_on(async {
                for handle in handles.drain(..) {
                    handle.await.unwrap();
                }
            });
        })
    });
}

fn rt_curr_yield_many_single_task(c: &mut Criterion) {
    let rt = rt();

    c.bench_function("yield_many_single_task", |b| {
        b.iter(|| {
            rt.block_on(async {
                for _ in 0..black_box(NUM_YIELD) {
                    tokio::task::yield_now().await;
                }
            });
        })
    });
}

fn rt() -> Runtime {
    runtime::Builder::new_current_thread().build().unwrap()
}

criterion_group!(
    rt_curr_scheduler,
    rt_curr_spawn_many_local,
    rt_curr_spawn_many_remote_idle,
    rt_curr_spawn_many_remote_busy,
    rt_curr_yield_many_single_task
);

criterion_main!(rt_curr_scheduler);
