#![feature(bench_black_box)]
use haphazard::*;
use statrs::statistics::OrderStatistics;

use std::io::Write;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::atomic::{AtomicPtr, AtomicUsize};
use std::sync::Arc;
use std::time::{Instant, Duration};

use rand::Rng;

struct CountDrops(Arc<AtomicUsize>);
impl Drop for CountDrops {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

static RUNNING: AtomicBool = AtomicBool::new(true);

fn main() {
    let mut rng = rand::thread_rng();

    let mut values = Vec::new();
    let ptr_count = 16 * 1024;

    const READER_HAZPTR_COUNT: usize = 1024 * 2;
    const READER_COUNT: usize = 4;
    const WRITER_COUNT: usize = 2;

    //Fill buffer with random values
    for _ in 0..ptr_count {
        let num = rng.gen::<usize>();
        let ptr = AtomicPtr::new(Box::into_raw(Box::new(
            HazPtrObjectWrapper::with_global_domain(num),
        )));
        values.push(ptr);
    }
    let values = Box::leak(values.into_boxed_slice());

    let writers: Vec<_> = (0..WRITER_COUNT)
        .map(|_| {
            std::thread::spawn(|| {
                let mut retire_count: usize = 0;
                let mut rng = rand::thread_rng();
                let mut times = Vec::new();
                let mut last_average = None;

                while RUNNING.load(Ordering::Relaxed) {
                    let start = Instant::now();
                    for _ in 0..values.len() {
                        let i = rng.gen_range(0..values.len());
                        let new_num = rng.gen::<usize>();
                        let new_ptr = Box::into_raw(Box::new(
                            HazPtrObjectWrapper::with_global_domain(new_num),
                        ));

                        let ptr = &values[i];
                        let now_garbage = ptr.swap(new_ptr, Ordering::AcqRel);
                        // Safety:
                        //
                        //  1. The pointer came from Box, so is valid.
                        //  2. The old value is no longer accessible.
                        //  3. The deleter is valid for Box types.
                        unsafe { now_garbage.retire(&deleters::drop_box) };
                        retire_count += 1;
                    }
                    let time = start.elapsed().as_nanos() as u64 as f64 / 1_000.0;
                    times.push(time);

                    const SAMPLE_SIZE: usize = 1024;
                    if times.len() % SAMPLE_SIZE == 0 {
                        let avg: f64 = &times[(times.len() - SAMPLE_SIZE)..].iter().sum::<f64>()
                            / SAMPLE_SIZE as f64;
                        if let Some(last) = last_average.take() {
                            println!("Last {}, now {}", last, avg);
                        }
                        last_average = Some(avg);
                    }
                }
                (retire_count, times)
            })
        })
        .collect();

    let readers: Vec<_> = (0..READER_COUNT)
        .map(|_| {
            std::thread::spawn(|| {
                let mut rng = rand::thread_rng();
                let mut sum: usize = 0;

                let mut hazptrs = [(); READER_HAZPTR_COUNT].map(|_| HazardPointer::make_global());
                let mut h_index = 0;

                while RUNNING.load(Ordering::Relaxed) {
                    for _ in 0..values.len() {
                        let i = rng.gen_range(0..values.len());
                        let h = &mut hazptrs[h_index];
                        h.reset_protection();

                        // Safety:
                        //
                        //  1. AtomicPtr points to a Box, so is always valid.
                        //  2. Writers to AtomicPtr use HazPtrObject::retire.
                        let ptr = unsafe { h.protect(&values[i]).unwrap() };
                        let val: usize = ptr.deref().clone();
                        sum = sum.wrapping_add(val);
                        h_index += 1;
                        h_index %= READER_HAZPTR_COUNT;
                    }
                }
                std::hint::black_box(sum);
            })
        })
        .collect();

    let start = Instant::now();
    let mut line = String::new();

    let end_with_user_input = false;
    if end_with_user_input {
        println!("Press enter to stop benchmark");
        std::io::stdin().read_line(&mut line).unwrap();
        println!();
    } else {
        //Run for 20 minutes
        println!("Running benchmark for 20 minutes");
        std::thread::sleep(Duration::from_secs(60 * 20));
    }
    RUNNING.store(false, Ordering::SeqCst);

    let mut retire_count = 0;
    let mut retire_times = Vec::new();

    for writer in writers {
        let (single_retire_count, single_retire_times) = writer.join().unwrap();
        retire_count += single_retire_count;
        retire_times.extend(single_retire_times);
    }
    let bench_duration = start.elapsed();

    println!(
        "Retired {} objects in {:.2}",
        retire_count,
        bench_duration.as_secs_f64()
    );

    let print_time = |data_micros: &Vec<f64>, method| {
        let retire_count = data_micros.len() * ptr_count;
        //The mean time for a run of `retire_count` retires
        let total_seconds = data_micros.iter().map(|micros| *micros).sum::<f64>() / 1_000_000.0;
        println!("{}:", method);

        println!("  {:.1} retire/sec", retire_count as f64 / total_seconds,);
        println!(
            "  {:.1} ns / retire",
            total_seconds * 1_000_000_000.0 / retire_count as f64
        );
    };

    //Retire times is in microseconds
    print_time(&retire_times, "Initial average");

    let write_data = |file_name, data: &Vec<f64>| {
        let mut data_file = std::fs::File::create(file_name).unwrap();
        writeln!(data_file, "time,").unwrap();
        for entry in data {
            //Time is the number of nanoseconds to perform `ptr_count` retire operations
            writeln!(data_file, "{},", *entry as f64 / ptr_count as f64).unwrap();
        }
    };

    write_data("initial_data.csv", &retire_times);

    let initial_length = retire_times.len();
    //Remove outliers until all data points are within 1.5 X the IQR
    loop {
        let mut data = statrs::statistics::Data::new(&mut retire_times);
        let mean = data.percentile(50);
        let lower = data.lower_quartile();
        let upper = data.upper_quartile();
        let iqr = upper - lower;
        let iqr_1_5 = iqr * 1.5;

        let before_len = retire_times.len();
        retire_times = retire_times
            .iter()
            .map(|t| *t)
            .filter(|t| (t - mean).abs() < iqr_1_5)
            .collect();

        if before_len == retire_times.len() {
            //We have removed all the outliers
            break;
        }
    }

    write_data("data.csv", &retire_times);

    let num_removed = initial_length - retire_times.len();
    println!(
        "Removed {} outliers, {:.1}%",
        num_removed,
        num_removed as f64 / initial_length as f64 * 100.0
    );

    print_time(&retire_times, "Adjusted average");

    for reader in readers {
        reader.join().unwrap();
    }

    /*
    let drops_42 = Arc::new(AtomicUsize::new(0));

    let x = AtomicPtr::new(Box::into_raw(Box::new(
        HazPtrObjectWrapper::with_global_domain((42, CountDrops(Arc::clone(&drops_42)))),
    )));

    // As a reader:
    let mut h = HazardPointer::make_global();

    // Safety:
    //
    //  1. AtomicPtr points to a Box, so is always valid.
    //  2. Writers to AtomicPtr use HazPtrObject::retire.
    let my_x = unsafe { h.protect(&x) }.expect("not null");
    // valid:
    assert_eq!(my_x.0, 42);
    h.reset_protection();
    // invalid:
    // let _: i32 = my_x.0;

    let my_x = unsafe { h.protect(&x) }.expect("not null");
    // valid:
    assert_eq!(my_x.0, 42);
    drop(h);
    // invalid:
    // let _: i32 = my_x.0;

    let mut h = HazardPointer::make_global();
    let my_x = unsafe { h.protect(&x) }.expect("not null");

    let mut h_tmp = HazardPointer::make_global();
    let _ = unsafe { h_tmp.protect(&x) }.expect("not null");
    drop(h_tmp);

    // As a writer:
    let drops_9001 = Arc::new(AtomicUsize::new(0));
    let old = x.swap(
        Box::into_raw(Box::new(HazPtrObjectWrapper::with_global_domain((
            9001,
            CountDrops(Arc::clone(&drops_9001)),
        )))),
        std::sync::atomic::Ordering::SeqCst,
    );

    let mut h2 = HazardPointer::make_global();
    let my_x2 = unsafe { h2.protect(&x) }.expect("not null");

    assert_eq!(my_x.0, 42);
    assert_eq!(my_x2.0, 9001);

    // Safety:
    //
    //  1. The pointer came from Box, so is valid.
    //  2. The old value is no longer accessible.
    //  3. The deleter is valid for Box types.
    unsafe { old.retire(&deleters::drop_box) };

    assert_eq!(drops_42.load(Ordering::SeqCst), 0);
    assert_eq!(my_x.0, 42);

    let n = Domain::global().eager_reclaim();
    assert_eq!(n, 0);

    assert_eq!(drops_42.load(Ordering::SeqCst), 0);
    assert_eq!(my_x.0, 42);

    drop(h);
    assert_eq!(drops_42.load(Ordering::SeqCst), 0);
    // _not_ drop(h2);

    let n = Domain::global().eager_reclaim();
    assert_eq!(n, 1);

    assert_eq!(drops_42.load(Ordering::SeqCst), 1);
    assert_eq!(drops_9001.load(Ordering::SeqCst), 0);

    drop(h2);
    let n = Domain::global().eager_reclaim();
    assert_eq!(n, 0);
    assert_eq!(drops_9001.load(Ordering::SeqCst), 0);
    */
}
