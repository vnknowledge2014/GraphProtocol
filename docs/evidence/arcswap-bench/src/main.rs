use arc_swap::ArcSwap;
use std::sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}};
use std::time::Instant;
struct Snap { v: Vec<u64> }
fn main() {
    let big = Arc::new(Snap { v: vec![1; 1_000_000] });
    let s = Arc::new(ArcSwap::from(big));
    // uncontended load()
    let n = 20_000_000u64; let t = Instant::now(); let mut acc = 0u64;
    for _ in 0..n { acc += s.load().v[0]; }
    println!("load() uncontended: {:.1} ns/op (acc={})", t.elapsed().as_nanos() as f64 / n as f64, acc);
    // store latency, no readers
    let mut lat = vec![];
    for _ in 0..2000 { let nx = Arc::new(Snap { v: vec![2; 1000] }); let t = Instant::now(); s.store(nx); lat.push(t.elapsed().as_nanos() as u64); }
    lat.sort(); println!("store() no readers: p50={}ns p99={}ns max={}ns", lat[lat.len()/2], lat[lat.len()*99/100], lat[lat.len()-1]);
    // store latency under N busy readers, each holding guards across a short critical section
    for readers in [8usize, 64, 256] {
        let stop = Arc::new(AtomicBool::new(false)); let cnt = Arc::new(AtomicU64::new(0));
        let hs: Vec<_> = (0..readers).map(|_| { let s=s.clone(); let stop=stop.clone(); let cnt=cnt.clone();
            std::thread::spawn(move || { while !stop.load(Ordering::Relaxed) { let g = s.load(); let mut x=0u64; for i in 0..50 { x = x.wrapping_add(g.v[i % g.v.len()]); } cnt.fetch_add(x & 1, Ordering::Relaxed); } }) }).collect();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let mut lat = vec![];
        for _ in 0..500 { let nx = Arc::new(Snap { v: vec![3; 1000] }); let t = Instant::now(); s.store(nx); lat.push(t.elapsed().as_nanos() as u64); std::thread::sleep(std::time::Duration::from_micros(200)); }
        stop.store(true, Ordering::Relaxed); for h in hs { h.join().unwrap(); }
        lat.sort(); println!("store() with {} busy readers: p50={}ns p99={}ns max={}ns", readers, lat[lat.len()/2], lat[lat.len()*99/100], lat[lat.len()-1]);
    }
    // cost of dropping a large old snapshot on the swapping thread
    let a = Arc::new(Snap { v: vec![9; 50_000_000] }); s.store(a);
    let t = Instant::now(); s.store(Arc::new(Snap { v: vec![0; 10] })); println!("store() that drops a 400MB snapshot: {} us", t.elapsed().as_micros());
}
