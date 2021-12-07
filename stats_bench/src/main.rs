#![feature(bench_black_box)]
use haphazard::*;

use std::io::Write;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::atomic::{AtomicPtr, AtomicUsize};
use std::time::{Duration, Instant};

use rand::Rng;

static RUNNING: AtomicBool = AtomicBool::new(true);
static TOTAL_RETIRED: AtomicUsize = AtomicUsize::new(0);

fn main() {
    //Drop privileges so that can create files normally, while having priority with the scheduler
    privdrop("troy", "troy").expect("Failed to drop privileges");

    const TRIALS: usize = 50;
    let mut trial_averages = Vec::new();
    for i in 0..TRIALS {
        let domain = Box::into_raw(Box::new(haphazard::Domain::new(&|| ())));
        // # Safety
        // 1. domain came from a box so it is valid and aligned
        // 2. We dont drop domain until after all haphazard pointers have been freed
        // 3. We only drop domain after all hazptrs have been deleted, so casting to 'static is
        //    safe, to make users of domain happy
        let domain_ref: &'static haphazard::Domain<_> = unsafe { &*domain };
        println!("Beginning trial #{}", i + 1);
        let mut rng = rand::thread_rng();

        //Inner scope. Create hazptrs only in here
        {
            let mut values = Vec::new();
            let ptr_count = 16 * 1024;

            const READER_HAZPTR_COUNT: usize = 1024 * 2;
            const READER_COUNT: usize = 4;
            const WRITER_COUNT: usize = 2;
            const CHECK_INTERVAL: usize = 1024 * 50;

            // How many retires we do in this benchmark
            const RETIRES_WANTED: usize = 50_000_000;

            //Fill buffer with random values
            for _ in 0..ptr_count {
                let num = rng.gen::<usize>();
                let ptr = AtomicPtr::new(Box::into_raw(Box::new(
                    HazPtrObjectWrapper::with_domain(domain_ref, num),
                )));
                values.push(ptr);
            }
            let values = Box::leak(values.into_boxed_slice());
            TOTAL_RETIRED.store(0, Ordering::SeqCst);
            RUNNING.store(true, Ordering::SeqCst);

            let writers: Vec<_> = (0..WRITER_COUNT)
                .map(|_| {
                    std::thread::spawn(|| {
                        let mut rng = rand::thread_rng();

                        let start = Instant::now();
                        while RUNNING.load(Ordering::Relaxed) {
                            let mut retire_count: usize = 0;
                            for _ in 0..CHECK_INTERVAL {
                                let i = rng.gen_range(0..values.len());
                                let new_num = rng.gen::<usize>();
                                let new_ptr = Box::into_raw(Box::new(
                                    HazPtrObjectWrapper::with_domain(domain_ref, new_num),
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
                            let total_retired =
                                TOTAL_RETIRED.fetch_add(retire_count, Ordering::SeqCst);
                            if total_retired > RETIRES_WANTED {
                                match RUNNING.compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst) {
                                    Ok(_) => break,
                                    Err(running) => {
                                        if running {
                                            panic!("Compare exchange failed and running set to true?");
                                        } else {
                                            //Someone else got here before us
                                            break;
                                        }
                                    }
                                }
                            }
                            let _ = std::io::stdout().flush();
                        }
                        let duration = start.elapsed();

                        duration
                    })
                })
                .collect();

            let readers: Vec<_> = (0..READER_COUNT)
                .map(|_| {
                    std::thread::spawn(|| {
                        let mut rng = rand::thread_rng();
                        let mut sum: usize = 0;

                        let mut hazptrs = [(); READER_HAZPTR_COUNT]
                            .map(|_| HazardPointer::make_in_domain(domain_ref));
                        let mut h_index = 0;

                        while RUNNING.load(Ordering::Relaxed) {
                            for _ in 0..CHECK_INTERVAL {
                                let i = rng.gen_range(0..values.len());
                                let h = &mut hazptrs[h_index];
                                h.reset_protection();

                                // Safety:
                                //
                                //  1. AtomicPtr points to a Box, so is always valid.
                                //  2. Writers to AtomicPtr use HazPtrObject::retire.
                                let ptr = unsafe { h.protect(&values[i]).unwrap() };
                                let val: usize = *ptr.deref();
                                sum = sum.wrapping_add(val);
                                h_index += 1;
                                h_index %= READER_HAZPTR_COUNT;
                            }
                        }
                        std::hint::black_box(sum);
                    })
                })
                .collect();

            while RUNNING.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(1000));
                let shard_counts = domain_ref.count_shards();
                for count in shard_counts {
                    print!("{} ", count);
                }
                let retired = TOTAL_RETIRED.load(Ordering::Relaxed);
                println!(" {:.1}% ({})", retired as f64 / RETIRES_WANTED as f64 * 100.0, retired);
            }

            for reader in readers {
                reader.join().unwrap();
            }

            let mut total_time = Duration::from_nanos(0);
            for writer in writers {
                let run_duration = writer.join().unwrap();
                total_time += run_duration;
            }

            let total_retired = TOTAL_RETIRED.load(Ordering::SeqCst);

            println!(
                "Retired {} objects in {:.2}",
                total_retired,
                total_time.as_secs_f64()
            );

            println!(
                "  {:.1} retire/sec",
                total_retired as f64 / total_time.as_secs_f64()
            );
            let ns_per_retire = total_time.as_secs_f64() * 1_000_000_000.0 / total_retired as f64;
            trial_averages.push(ns_per_retire);
            println!("  {:.1} ns / retire", ns_per_retire,);
        }
        // # Safety
        // All threads using hazptrs have stopped and all hazptrs have been dropped
        let domain_ref = unsafe { &mut *domain };
        domain_ref.reclaim_all_objects();
        //Make sure all objects have been dropped since we are about to drop the domain
        assert_eq!(domain_ref.count_shards(), [0; 8]);

        // # Safety
        // 1. We have exclusive access to domain, since all users have stopped or gone out of scope
        // 2. By the assert above, there are no outlying to-be-retired objects
        let _ = unsafe { Box::from_raw(domain) };
    }

    let file_name = "data.csv";
    let mut data_file = std::fs::File::create(file_name).unwrap();
    writeln!(data_file, "time,").unwrap();
    for entry in &trial_averages {
        //Time is the number of nanoseconds to perform `ptr_count` retire operations
        writeln!(data_file, "{},", entry,).unwrap();
    }
    println!(
        "Wrote {} data points to {}",
        trial_averages.len(),
        file_name
    );
}

use std::ffi::CString;
use std::io::{Error, ErrorKind};

// Code obtained from: https://blog.lxsang.me/post/id/28
pub fn privdrop(user: &str, group: &str) -> Result<(), Error> {
    // the group id need to be set first, otherwise,
    // when the user privileges drop, it is unnable to
    // drop the group privileges
    if let Ok(cstr) = CString::new(group.as_bytes()) {
        let p = unsafe { libc::getgrnam(cstr.as_ptr()) };

        if p.is_null() {
            eprintln!("privdrop: Unable to getgrnam of group: {}", group);
            return Err(Error::last_os_error());
        }

        if unsafe { libc::setgid((*p).gr_gid) } != 0 {
            eprintln!("privdrop: Unable to setgid of group: {}", group);
            return Err(Error::last_os_error());
        }
    } else {
        return Err(Error::new(
            ErrorKind::Other,
            "Cannot create CString from String (group)!",
        ));
    }

    // drop the user privileges
    if let Ok(cstr) = CString::new(user.as_bytes()) {
        let p = unsafe { libc::getpwnam(cstr.as_ptr()) };

        if p.is_null() {
            eprintln!("privdrop: Unable to getpwnam of user: {}", user);
            return Err(Error::last_os_error());
        }

        if unsafe { libc::setuid((*p).pw_uid) } != 0 {
            eprintln!("privdrop: Unable to setuid of user: {}", user);
            return Err(Error::last_os_error());
        }
    } else {
        return Err(Error::new(
            ErrorKind::Other,
            "Cannot create CString from String (user)!",
        ));
    }

    Ok(())
}
