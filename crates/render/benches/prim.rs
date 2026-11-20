use std::{fmt, hint::black_box};

use render::scene::Detail;
use world::PrimShape;

use crate::support;

#[derive(Clone, Copy, Debug)]
struct PrimCase {
    shape: &'static str,
    detail: &'static str,
}

const CASES: &[PrimCase] = &[
    PrimCase { shape: "box", detail: "Lowest" },
    PrimCase { shape: "box", detail: "Low" },
    PrimCase { shape: "box", detail: "Medium" },
    PrimCase { shape: "box", detail: "High" },
    PrimCase { shape: "cylinder", detail: "Lowest" },
    PrimCase { shape: "cylinder", detail: "Low" },
    PrimCase { shape: "cylinder", detail: "Medium" },
    PrimCase { shape: "cylinder", detail: "High" },
    PrimCase { shape: "sphere", detail: "Lowest" },
    PrimCase { shape: "sphere", detail: "Low" },
    PrimCase { shape: "sphere", detail: "Medium" },
    PrimCase { shape: "sphere", detail: "High" },
    PrimCase { shape: "torus", detail: "Lowest" },
    PrimCase { shape: "torus", detail: "Low" },
    PrimCase { shape: "torus", detail: "Medium" },
    PrimCase { shape: "torus", detail: "High" },
];

fn shape(name: &str) -> PrimShape {
    match name {
        "box" => PrimShape::default(),
        "cylinder" => PrimShape { profile_curve: 0, ..PrimShape::default() },
        "sphere" => PrimShape { path_curve: 0x20, profile_curve: 5, ..PrimShape::default() },
        "torus" => PrimShape { path_curve: 0x20, profile_curve: 0, path_scale_x: 150, path_scale_y: 150, ..PrimShape::default() },
        _ => unreachable!("known prim shape"),
    }
}

fn detail(name: &str) -> Detail {
    match name {
        "Lowest" => Detail::Lowest,
        "Low" => Detail::Low,
        "Medium" => Detail::Medium,
        "High" => Detail::High,
        _ => unreachable!("known prim detail"),
    }
}

impl fmt::Display for PrimCase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.shape, self.detail)
    }
}

mod benches {
    use super::*;

    #[divan::bench(args = CASES)]
    fn scene_prim_mesh_generate(bencher: divan::Bencher, case: PrimCase) {
        let shape = shape(case.shape);
        let detail = detail(case.detail);
        bencher.with_inputs(|| support::prim_job(shape, detail)).bench_local_values(|(reconciler, job)| {
            black_box(job.run());
            black_box(reconciler);
        });
    }
}
