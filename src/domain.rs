//! This module contains an `EvaluationDomain` abstraction for
//! performing various kinds of polynomial arithmetic on top of
//! the scalar field.
//!
//! In pairing-based SNARKs like Groth16, we need to calculate
//! a quotient polynomial over a target polynomial with roots
//! at distinct points associated with each constraint of the
//! constraint system. In order to be efficient, we choose these
//! roots to be the powers of a 2^n root of unity in the field.
//! This allows us to perform polynomial operations in O(n)
//! by performing an O(n log n) FFT over such a domain.

use pairing::{
    Engine,
    CurveProjective
};

use ff::{
    Field, 
    PrimeField
};

use super::{
    SynthesisError
};


use super::multicore::Worker;
pub use super::group::*;

#[cfg(not(feature = "singlecore"))]
use super::parallel_fft::*;

pub struct EvaluationDomain<E: Engine, G: Group<E>> {
    coeffs: Vec<G>,
    exp: u32,
    omega: E::Fr,
    omegainv: E::Fr,
    geninv: E::Fr,
    minv: E::Fr
}

impl<E: Engine, G: Group<E>> EvaluationDomain<E, G> {
    pub fn as_ref(&self) -> &[G] {
        &self.coeffs
    }

    pub fn as_mut(&mut self) -> &mut [G] {
        &mut self.coeffs
    }

    pub fn into_coeffs(self) -> Vec<G> {
        self.coeffs
    }

    pub fn from_coeffs(mut coeffs: Vec<G>) -> Result<EvaluationDomain<E, G>, SynthesisError>
    {
        use ff::PrimeField;
        // Compute the size of our evaluation domain

        let coeffs_len = coeffs.len();

        // m is a size of domain where Z polynomial does NOT vanish
        // in normal domain Z is in a form of (X-1)(X-2)...(X-N)
        let mut m = 1;
        let mut exp = 0;
        let mut omega = E::Fr::root_of_unity();
        let max_degree = (1 << E::Fr::S) - 1;

        if coeffs_len > max_degree {
            return Err(SynthesisError::PolynomialDegreeTooLarge)
        }

        while m < coeffs_len {
            m *= 2;
            exp += 1;

            // The pairing-friendly curve may not be able to support
            // large enough (radix2) evaluation domains.
            if exp > E::Fr::S {
                return Err(SynthesisError::PolynomialDegreeTooLarge)
            }
        }

        // If full domain is not needed - limit it,
        // e.g. if (2^N)th power is not required, just double omega and get 2^(N-1)th
        // Compute omega, the 2^exp primitive root of unity
        for _ in exp..E::Fr::S {
            omega.square();
        }

        // Extend the coeffs vector with zeroes if necessary
        coeffs.resize(m, G::group_zero());

        Ok(EvaluationDomain {
            coeffs: coeffs,
            exp: exp,
            omega: omega,
            omegainv: omega.inverse().unwrap(),
            geninv: E::Fr::multiplicative_generator().inverse().unwrap(),
            minv: E::Fr::from_str(&format!("{}", m)).unwrap().inverse().unwrap()
        })
    }

    // this one does expect coefficients to be smaller than `num_roots_of_unity/2` as we expect multiplication
    pub fn from_coeffs_into_sized(mut coeffs: Vec<G>, size: usize) -> Result<EvaluationDomain<E, G>, SynthesisError>
    {
        use ff::PrimeField;
        // Compute the size of our evaluation domain

        assert!(size >= coeffs.len());

        let coeffs_len = size;

        // m is a size of domain where Z polynomial does NOT vanish
        // in normal domain Z is in a form of (X-1)(X-2)...(X-N)
        let mut m = 1;
        let mut exp = 0;
        let mut omega = E::Fr::root_of_unity();
        let max_degree = (1 << E::Fr::S) - 1;

        if coeffs_len > max_degree {
            return Err(SynthesisError::PolynomialDegreeTooLarge)
        }

        while m < coeffs_len {
            m *= 2;
            exp += 1;

            // The pairing-friendly curve may not be able to support
            // large enough (radix2) evaluation domains.
            if exp > E::Fr::S {
                return Err(SynthesisError::PolynomialDegreeTooLarge)
            }
        }

        // If full domain is not needed - limit it,
        // e.g. if (2^N)th power is not required, just double omega and get 2^(N-1)th
        // Compute omega, the 2^exp primitive root of unity
        for _ in exp..E::Fr::S {
            omega.square();
        }

        // Extend the coeffs vector with zeroes if necessary
        coeffs.resize(m, G::group_zero());

        Ok(EvaluationDomain {
            coeffs: coeffs,
            exp: exp,
            omega: omega,
            omegainv: omega.inverse().unwrap(),
            geninv: E::Fr::multiplicative_generator().inverse().unwrap(),
            minv: E::Fr::from_str(&format!("{}", m)).unwrap().inverse().unwrap()
        })
    }


    pub fn fft(&mut self, worker: &Worker)
    {
        best_fft(&mut self.coeffs, worker, &self.omega, self.exp);
    }

    pub fn ifft(&mut self, worker: &Worker)
    {
        best_fft(&mut self.coeffs, worker, &self.omegainv, self.exp);

        worker.scope(self.coeffs.len(), |scope, chunk| {
            let minv = self.minv;

            for v in self.coeffs.chunks_mut(chunk) {
                scope.spawn(move |_| {
                    for v in v {
                        v.group_mul_assign(&minv);
                    }
                });
            }
        });
    }

    pub fn distribute_powers(&mut self, worker: &Worker, g: E::Fr)
    {
        worker.scope(self.coeffs.len(), |scope, chunk| {
            for (i, v) in self.coeffs.chunks_mut(chunk).enumerate() {
                scope.spawn(move |_| {
                    let mut u = g.pow(&[(i * chunk) as u64]);
                    for v in v.iter_mut() {
                        v.group_mul_assign(&u);
                        u.mul_assign(&g);
                    }
                });
            }
        });
    }

    pub fn coset_fft(&mut self, worker: &Worker)
    {
        self.distribute_powers(worker, E::Fr::multiplicative_generator());
        self.fft(worker);
    }

    pub fn icoset_fft(&mut self, worker: &Worker)
    {
        let geninv = self.geninv;

        self.ifft(worker);
        self.distribute_powers(worker, geninv);
    }

    /// This evaluates t(tau) for this domain, which is
    /// tau^m - 1 for these radix-2 domains.
    pub fn z(&self, tau: &E::Fr) -> E::Fr {
        let mut tmp = tau.pow(&[self.coeffs.len() as u64]);
        tmp.sub_assign(&E::Fr::one());

        tmp
    }

    /// The target polynomial is the zero polynomial in our
    /// evaluation domain, so we must perform division over
    /// a coset.
    pub fn divide_by_z_on_coset(&mut self, worker: &Worker)
    {
        let i = self.z(&E::Fr::multiplicative_generator()).inverse().unwrap();

        worker.scope(self.coeffs.len(), |scope, chunk| {
            for v in self.coeffs.chunks_mut(chunk) {
                scope.spawn(move |_| {
                    for v in v {
                        v.group_mul_assign(&i);
                    }
                });
            }
        });
    }

    /// Perform O(n) multiplication of two polynomials in the domain.
    pub fn mul_assign(&mut self, worker: &Worker, other: &EvaluationDomain<E, Scalar<E>>) {
        assert_eq!(self.coeffs.len(), other.coeffs.len());

        worker.scope(self.coeffs.len(), |scope, chunk| {
            for (a, b) in self.coeffs.chunks_mut(chunk).zip(other.coeffs.chunks(chunk)) {
                scope.spawn(move |_| {
                    for (a, b) in a.iter_mut().zip(b.iter()) {
                        a.group_mul_assign(&b.0);
                    }
                });
            }
        });
    }

    /// Perform O(n) subtraction of one polynomial from another in the domain.
    pub fn sub_assign(&mut self, worker: &Worker, other: &EvaluationDomain<E, G>) {
        assert_eq!(self.coeffs.len(), other.coeffs.len());

        worker.scope(self.coeffs.len(), |scope, chunk| {
            for (a, b) in self.coeffs.chunks_mut(chunk).zip(other.coeffs.chunks(chunk)) {
                scope.spawn(move |_| {
                    for (a, b) in a.iter_mut().zip(b.iter()) {
                        a.group_sub_assign(&b);
                    }
                });
            }
        });
    }
}

// Test multiplying various (low degree) polynomials together and
// comparing with naive evaluations.
#[test]
fn polynomial_arith() {
    use pairing::bls12_381::Bls12;
    use rand::{self, Rand};

    fn test_mul<E: Engine, R: rand::Rng>(rng: &mut R)
    {
        let worker = Worker::new();

        for coeffs_a in 0..70 {
            for coeffs_b in 0..70 {
                let mut a: Vec<_> = (0..coeffs_a).map(|_| Scalar::<E>(E::Fr::rand(rng))).collect();
                let mut b: Vec<_> = (0..coeffs_b).map(|_| Scalar::<E>(E::Fr::rand(rng))).collect();

                // naive evaluation
                let mut naive = vec![Scalar(E::Fr::zero()); coeffs_a + coeffs_b];
                for (i1, a) in a.iter().enumerate() {
                    for (i2, b) in b.iter().enumerate() {
                        let mut prod = *a;
                        prod.group_mul_assign(&b.0);
                        naive[i1 + i2].group_add_assign(&prod);
                    }
                }

                a.resize(coeffs_a + coeffs_b, Scalar(E::Fr::zero()));
                b.resize(coeffs_a + coeffs_b, Scalar(E::Fr::zero()));

                let mut a = EvaluationDomain::from_coeffs(a).unwrap();
                let mut b = EvaluationDomain::from_coeffs(b).unwrap();

                a.fft(&worker);
                b.fft(&worker);
                a.mul_assign(&worker, &b);
                a.ifft(&worker);

                for (naive, fft) in naive.iter().zip(a.coeffs.iter()) {
                    assert!(naive == fft);
                }
            }
        }
    }

    let rng = &mut rand::thread_rng();

    test_mul::<Bls12, _>(rng);
}

#[test]
fn fft_composition() {
    use pairing::bls12_381::Bls12;
    use rand;

    fn test_comp<E: Engine, R: rand::Rng>(rng: &mut R)
    {
        let worker = Worker::new();

        for coeffs in 0..10 {
            let coeffs = 1 << coeffs;

            let mut v = vec![];
            for _ in 0..coeffs {
                v.push(Scalar::<E>(rng.gen()));
            }

            let mut domain = EvaluationDomain::from_coeffs(v.clone()).unwrap();
            domain.ifft(&worker);
            domain.fft(&worker);
            assert!(v == domain.coeffs);
            domain.fft(&worker);
            domain.ifft(&worker);
            assert!(v == domain.coeffs);
            domain.icoset_fft(&worker);
            domain.coset_fft(&worker);
            assert!(v == domain.coeffs);
            domain.coset_fft(&worker);
            domain.icoset_fft(&worker);
            assert!(v == domain.coeffs);
        }
    }

    let rng = &mut rand::thread_rng();

    test_comp::<Bls12, _>(rng);
}

#[test]
fn parallel_fft_consistency() {
    use pairing::bls12_381::Bls12;
    use rand::{self, Rand};
    use std::cmp::min;

    fn test_consistency<E: Engine, R: rand::Rng>(rng: &mut R)
    {
        let worker = Worker::new();

        for _ in 0..5 {
            for log_d in 0..10 {
                let d = 1 << log_d;

                let v1 = (0..d).map(|_| Scalar::<E>(E::Fr::rand(rng))).collect::<Vec<_>>();
                let mut v1 = EvaluationDomain::from_coeffs(v1).unwrap();
                let mut v2 = EvaluationDomain::from_coeffs(v1.coeffs.clone()).unwrap();

                for log_cpus in log_d..min(log_d+1, 3) {
                    parallel_fft(&mut v1.coeffs, &worker, &v1.omega, log_d, log_cpus);
                    serial_fft(&mut v2.coeffs, &v2.omega, log_d);

                    assert!(v1.coeffs == v2.coeffs);
                }
            }
        }
    }

    let rng = &mut rand::thread_rng();

    test_consistency::<Bls12, _>(rng);
}

#[test]
fn test_field_element_multiplication_bn256() {
    use rand::{self, Rand};
    use pairing::bn256::Bn256;
    use pairing::bn256::Fr;
    use num_cpus;

    let cpus = num_cpus::get();
    const SAMPLES: usize = 1 << 22;

    let rng = &mut rand::thread_rng();
    let v1 = (0..SAMPLES).map(|_| Scalar::<Bn256>(Fr::rand(rng))).collect::<Vec<_>>();
    let v2 = (0..SAMPLES).map(|_| Scalar::<Bn256>(Fr::rand(rng))).collect::<Vec<_>>();

    let mut v1 = EvaluationDomain::from_coeffs(v1).unwrap();
    let v2 = EvaluationDomain::from_coeffs(v2).unwrap();

    let pool = Worker::new();

    let start = std::time::Instant::now();

    v1.mul_assign(&pool, &v2);

    let duration_ns = start.elapsed().as_nanos() as f64;
    println!("Elapsed {} ns for {} samples", duration_ns, SAMPLES);
    let time_per_sample = duration_ns/(SAMPLES as f64);
    println!("Tested on {} samples on {} CPUs with {} ns per field element multiplication", SAMPLES, cpus, time_per_sample);
}

#[test]
fn test_fft_bn256() {
    use rand::{self, Rand};
    use pairing::bn256::Bn256;
    use pairing::bn256::Fr;
    use num_cpus;

    let cpus = num_cpus::get();
    const SAMPLES: usize = 1 << 27;

    let rng = &mut rand::thread_rng();
    let v1 = (0..SAMPLES).map(|_| Scalar::<Bn256>(Fr::rand(rng))).collect::<Vec<_>>();

    let mut v1 = EvaluationDomain::from_coeffs(v1).unwrap();

    let pool = Worker::new();

    let start = std::time::Instant::now();

    v1.ifft(&pool);

    let duration_ns = start.elapsed().as_nanos() as f64;
    println!("Elapsed {} ns for {} samples", duration_ns, SAMPLES);
    let time_per_sample = duration_ns/(SAMPLES as f64);
    println!("Tested on {} samples on {} CPUs with {} ns per field element multiplication", SAMPLES, cpus, time_per_sample);
}