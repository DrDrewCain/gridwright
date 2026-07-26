//! Scratch phase-timing benchmark for `from_csr_par`. Not a correctness test.
//! Run with:
//!   GW_CSC_TIMING=1 cargo test --profile bench-rel -p gridwright-model \
//!     --test transpose_phase_bench -- --ignored --nocapture --test-threads=1

use gridwright_model::csc::from_csr_par;
use rayon::prelude::*;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

const T: usize = 8760;
const ROW_ENTITIES: usize = 704;
const COL_BLOCKS: usize = 1856;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }
}

/// Entity-major CSR mirroring the builder: each row entity emits 8760
/// consecutive rows (one per hour); a row touches a fixed set of variable
/// blocks at offset +t, so each column stream advances by one per row.
fn gen_blocked() -> (Vec<u32>, Vec<u32>, Vec<f64>, usize) {
    let ks = [2usize, 3, 3, 4, 5, 6, 10];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let entity_blocks: Vec<Vec<u32>> = (0..ROW_ENTITIES)
        .map(|e| {
            (0..ks[e % ks.len()])
                .map(|_| (rng.next() as usize % COL_BLOCKS) as u32)
                .collect()
        })
        .collect();
    let nnz: usize = entity_blocks.iter().map(Vec::len).sum::<usize>() * T;
    let mut row_starts = Vec::with_capacity(ROW_ENTITIES * T + 1);
    row_starts.push(0u32);
    let mut cols = Vec::with_capacity(nnz);
    let mut vals = Vec::with_capacity(nnz);
    for blocks in &entity_blocks {
        for t in 0..T {
            for &b in blocks {
                cols.push(b * T as u32 + t as u32);
                vals.push((cols.len() % 1000) as f64 * 0.5);
            }
            row_starts.push(cols.len() as u32);
        }
    }
    (row_starts, cols, vals, COL_BLOCKS * T)
}

/// Same shape, uniformly random columns: the no-locality strawman.
fn gen_uniform(template: &(Vec<u32>, Vec<u32>, Vec<f64>, usize)) -> (Vec<u32>, Vec<u32>, Vec<f64>, usize) {
    let (row_starts, cols, vals, n_cols) = template;
    let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
    let rand_cols: Vec<u32> = cols
        .iter()
        .map(|_| (rng.next() as usize % n_cols) as u32)
        .collect();
    (row_starts.clone(), rand_cols, vals.clone(), *n_cols)
}

fn touch_u32(v: &mut [u32]) {
    v.par_chunks_mut(1 << 16).for_each(|c| {
        for x in c.iter_mut() {
            unsafe { std::ptr::write_volatile(x, 0) };
        }
    });
}
fn touch_f64(v: &mut [f64]) {
    v.par_chunks_mut(1 << 16).for_each(|c| {
        for x in c.iter_mut() {
            unsafe { std::ptr::write_volatile(x, 0.0) };
        }
    });
}

struct SendPtr<T>(*mut T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

#[repr(C)]
struct Timeval {
    tv_sec: i64,
    tv_usec: i32,
    _pad: i32,
}
#[repr(C)]
struct Rusage {
    ru_utime: Timeval,
    ru_stime: Timeval,
    ru_rest: [i64; 14],
}
unsafe extern "C" {
    fn getrusage(who: i32, usage: *mut Rusage) -> i32;
}
fn minflt() -> i64 {
    let mut r = Rusage {
        ru_utime: Timeval { tv_sec: 0, tv_usec: 0, _pad: 0 },
        ru_stime: Timeval { tv_sec: 0, tv_usec: 0, _pad: 0 },
        ru_rest: [0; 14],
    };
    unsafe { getrusage(0, &mut r) };
    r.ru_rest[4] // ru_minflt on macOS
}

/// Reimplementation of from_csr_par's phases. With `pre_touch` all memory is
/// pre-faulted so page-fault cost is reported separately from compute; without
/// it, phases fault their pages as the shipped code does. Reports minor page
/// faults per phase either way.
fn decomposed(
    row_starts: &[u32],
    cols: &[u32],
    vals: &[f64],
    n_cols: usize,
    label: &str,
    pre_touch: bool,
) {
    let n_rows = row_starts.len() - 1;
    let nnz = cols.len();
    let threads = rayon::current_num_threads().max(1);
    let chunk_rows = n_rows.div_ceil(threads).max(4096);
    let n_chunks = n_rows.div_ceil(chunk_rows).max(1);

    println!(
        "-- decomposed ({label}, pre_touch={pre_touch}), {threads} threads, {n_chunks} chunks --"
    );
    let mut lap_f = minflt();
    let mut lap = |name: &str, dt: std::time::Duration, lap_f: &mut i64| {
        let f = minflt();
        println!("  {name:22} {dt:>12.3?}   (+{} minflt)", f - *lap_f);
        *lap_f = f;
    };

    let t = Instant::now();
    let mut counts_raw = vec![0u32; n_cols];
    lap("alloc counts (mmap)", t.elapsed(), &mut lap_f);
    if pre_touch {
        let t = Instant::now();
        touch_u32(&mut counts_raw);
        lap("fault counts (65MB)", t.elapsed(), &mut lap_f);
    }
    let counts: Vec<AtomicU32> =
        unsafe { std::mem::transmute::<Vec<u32>, Vec<AtomicU32>>(counts_raw) };

    let t = Instant::now();
    cols.par_iter().for_each(|&c| {
        counts[c as usize].fetch_add(1, Ordering::Relaxed);
    });
    lap("count", t.elapsed(), &mut lap_f);

    // Parallel scan, identical structure to the shipped code.
    let t = Instant::now();
    let scan_chunk = n_cols.div_ceil(threads).max(1 << 16);
    #[allow(clippy::needless_range_loop)]
    let n_scan = n_cols.div_ceil(scan_chunk).max(1);
    let mut chunk_total = vec![0u32; n_scan];
    chunk_total.par_iter_mut().enumerate().for_each(|(chunk, total)| {
        let c0 = chunk * scan_chunk;
        let c1 = ((chunk + 1) * scan_chunk).min(n_cols);
        let mut sum = 0u32;
        for j in c0..c1 {
            sum += counts[j].load(Ordering::Relaxed);
        }
        *total = sum;
    });
    let mut base = Vec::with_capacity(n_scan);
    let mut running = 0u32;
    for &tt in &chunk_total {
        base.push(running);
        running += tt;
    }
    assert_eq!(running as usize, nnz);
    let mut starts = vec![0u32; n_cols + 1];
    if pre_touch {
        touch_u32(&mut starts);
    }
    starts[n_cols] = running;
    starts[..n_cols]
        .par_chunks_mut(scan_chunk)
        .enumerate()
        .for_each(|(chunk, out)| {
            let c0 = chunk * scan_chunk;
            let mut running = base[chunk];
            for (k, slot) in out.iter_mut().enumerate() {
                *slot = running;
                running += counts[c0 + k].load(Ordering::Relaxed);
            }
        });
    counts
        .par_iter()
        .enumerate()
        .for_each(|(j, slot)| slot.store(starts[j], Ordering::Relaxed));
    lap("scan+reset", t.elapsed(), &mut lap_f);

    let t = Instant::now();
    let mut out_rows = vec![0u32; nnz];
    let mut out_vals = vec![0.0f64; nnz];
    lap("alloc out (mmap)", t.elapsed(), &mut lap_f);
    if pre_touch {
        let t = Instant::now();
        touch_u32(&mut out_rows);
        touch_f64(&mut out_vals);
        lap("fault out (350MB)", t.elapsed(), &mut lap_f);
    }

    let t = Instant::now();
    {
        let rows_ptr = SendPtr(out_rows.as_mut_ptr());
        let vals_ptr = SendPtr(out_vals.as_mut_ptr());
        let counts = &counts;
        (0..n_chunks).into_par_iter().for_each(|chunk| {
            let r0 = chunk * chunk_rows;
            let r1 = ((chunk + 1) * chunk_rows).min(n_rows);
            let rows_ptr = &rows_ptr;
            let vals_ptr = &vals_ptr;
            for r in r0..r1 {
                let s = row_starts[r] as usize;
                let e = row_starts[r + 1] as usize;
                for k in s..e {
                    let c = cols[k] as usize;
                    let dst = counts[c].fetch_add(1, Ordering::Relaxed) as usize;
                    unsafe {
                        *rows_ptr.0.add(dst) = r as u32;
                        *vals_ptr.0.add(dst) = vals[k];
                    }
                }
            }
        });
    }
    lap("scatter", t.elapsed(), &mut lap_f);

    let t = Instant::now();
    {
        let rows_ptr = SendPtr(out_rows.as_mut_ptr());
        let vals_ptr = SendPtr(out_vals.as_mut_ptr());
        let starts_ref = &starts;
        (0..n_cols).into_par_iter().with_min_len(4096).for_each(|j| {
            let s = starts_ref[j] as usize;
            let e = starts_ref[j + 1] as usize;
            if e <= s + 1 {
                return;
            }
            let rows_ptr = &rows_ptr;
            let vals_ptr = &vals_ptr;
            unsafe {
                let r = std::slice::from_raw_parts_mut(rows_ptr.0.add(s), e - s);
                let v = std::slice::from_raw_parts_mut(vals_ptr.0.add(s), e - s);
                for i in 1..r.len() {
                    let (ri, vi) = (r[i], v[i]);
                    let mut k = i;
                    while k > 0 && r[k - 1] > ri {
                        r[k] = r[k - 1];
                        v[k] = v[k - 1];
                        k -= 1;
                    }
                    r[k] = ri;
                    v[k] = vi;
                }
            }
        });
    }
    lap("sort", t.elapsed(), &mut lap_f);
    std::hint::black_box((&out_rows, &out_vals, &starts));
}

/// How fast can this machine fault fresh zero-fill pages, serial vs parallel?
fn fault_rate_probe() {
    let n = 29_013_120usize; // 233 MB of f64
    let mut a = vec![0.0f64; n];
    let mut b = vec![0.0f64; n];
    let t = Instant::now();
    for i in (0..n).step_by(2048) {
        unsafe { std::ptr::write_volatile(a.as_mut_ptr().add(i), 1.0) };
    }
    let serial = t.elapsed();
    std::hint::black_box(&a);
    let t = Instant::now();
    b.par_chunks_mut(1 << 16).for_each(|c| {
        for i in (0..c.len()).step_by(2048) {
            unsafe { std::ptr::write_volatile(c.as_mut_ptr().add(i), 1.0) };
        }
    });
    let par = t.elapsed();
    println!(
        "-- fault probe, 233MB fresh, one write per 16KB page: serial {serial:?}, 14-thread {par:?} --"
    );
    std::hint::black_box(&b);
}

fn bw_reference() {
    // Achievable parallel copy bandwidth, as a yardstick.
    let n = 29_013_120usize;
    let src = vec![1.0f64; n];
    let mut dst = vec![0.0f64; n];
    touch_f64(&mut dst);
    let t = Instant::now();
    dst.par_chunks_mut(1 << 16)
        .zip(src.par_chunks(1 << 16))
        .for_each(|(d, s)| d.copy_from_slice(s));
    let dt = t.elapsed();
    let gb = (2 * n * 8) as f64 / 1e9;
    println!(
        "-- memcpy reference: {:?} for {:.2} GB touched = {:.0} GB/s --",
        dt,
        gb,
        gb / dt.as_secs_f64()
    );
    std::hint::black_box(&dst);
}

#[test]
#[ignore]
fn phase_bench() {
    let blocked = gen_blocked();
    let (row_starts, cols, vals, n_cols) = &blocked;
    let n_rows = row_starts.len() - 1;
    let nnz = cols.len();
    println!(
        "shape: n_rows={} n_cols={} nnz={} threads={}",
        n_rows,
        n_cols,
        nnz,
        rayon::current_num_threads()
    );

    if std::env::var_os("GW_BW_FIRST").is_some() {
        bw_reference();
    }
    if std::env::var_os("GW_SKIP_PRE").is_none() {
        // Truly fresh pages: nothing large freed yet in this process.
        decomposed(row_starts, cols, vals, *n_cols, "blocked, first call", false);
        // Steady state: allocator now holds the just-freed regions.
        decomposed(row_starts, cols, vals, *n_cols, "blocked, steady", false);
    }

    println!("== from_csr_par, blocked (realistic) columns ==");
    for i in 0..4 {
        let t = Instant::now();
        let csc = from_csr_par(row_starts, cols, vals, *n_cols);
        println!("iter {i}: total {:?}", t.elapsed());
        std::hint::black_box(&csc);
    }

    decomposed(row_starts, cols, vals, *n_cols, "blocked", true);
    fault_rate_probe();
    bw_reference();

    let uniform = gen_uniform(&blocked);
    println!("== from_csr_par, uniform random columns ==");
    for i in 0..2 {
        let t = Instant::now();
        let csc = from_csr_par(&uniform.0, &uniform.1, &uniform.2, uniform.3);
        println!("iter {i}: total {:?}", t.elapsed());
        std::hint::black_box(&csc);
    }
    decomposed(&uniform.0, &uniform.1, &uniform.2, uniform.3, "uniform, steady", false);
    decomposed(&uniform.0, &uniform.1, &uniform.2, uniform.3, "uniform", true);
}
