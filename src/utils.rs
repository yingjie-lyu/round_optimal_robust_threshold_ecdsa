use bicycl::{CL_HSMqk, CipherText, ClearText, Mpz, PublicKey, RandGen, SecretKey, QFI};
use curv::{
    arithmetic::{BasicOps, Converter, Samplable},
    cryptographic_primitives::hashing::merkle_tree::Proof,
    elliptic::curves::{Point, Scalar, Secp256k1},
    BigInt,
};
use ecdsa::elliptic_curve::point;
use futures::SinkExt;
use round_based::{
    rounds_router::simple_store::RoundInput, rounds_router::RoundsRouter, simulation::Simulation,
    Delivery, Mpc, MpcParty, Outgoing, PartyIndex, ProtocolMessage,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, ops::Deref};
use std::{
    error::Error,
    ops::{Add, Mul},
};
use thiserror::Error;

use rayon::prelude::*;

pub type Zq = Scalar<Secp256k1>;
pub type G = Point<Secp256k1>;
pub type Id = u8;

/// Polynomial defined over Zq, with coefficients in ascending order
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Polynomial {
    pub coeffs: Vec<Zq>,
}

impl Polynomial {
    pub fn new(degree: Id, some_coeffs: &BTreeMap<Id, Zq>) -> Self {
        let mut coeffs = vec![Zq::zero(); degree as usize + 1];
        some_coeffs
            .iter()
            .take_while(|(&id, _)| id <= degree)
            .for_each(|(&id, coeff)| coeffs[id as usize] = coeff.clone());
        Self { coeffs }
    }

    pub fn eval(&self, x: &Zq) -> Zq {
        let mut result = Zq::zero();
        for i in (0..self.coeffs.len()).rev() {
            result = result * x + &self.coeffs[i];
        }
        result
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CurvePolynomial {
    pub coeffs: Vec<G>,
}

impl CurvePolynomial {
    // trivial constructor makes very little sense.
    // should refactor to take a BTreeMap<Id, G> instead
    pub fn new(degree: Id, some_coeffs: &BTreeMap<Id, G>) -> Self {
        let mut coeffs = vec![G::zero(); degree as usize + 1];
        some_coeffs
            .iter()
            .take_while(|(&id, _)| id <= degree)
            .for_each(|(&id, coeff)| coeffs[id as usize] = coeff.clone());
        Self { coeffs }
    }

    pub fn from_exp(polynomial: &Polynomial, generator: &G) -> Self {
        Self {
            coeffs: polynomial
                .coeffs
                .par_iter()
                .map(|x| generator * x)
                .collect(),
        }
    }

    pub fn eval(&self, x: &Zq) -> G {
        let mut result = G::zero();
        for i in (0..self.coeffs.len()).rev() {
            result = result * x + &self.coeffs[i];
        }
        result
    }
}

/// TODO: refactor to use the `CurvePolynomial` struct
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QFPolynomial {
    pub coeffs: Vec<QFI>,
}

impl QFPolynomial {
    /// Creates a class group polynomial with only some coefficients specified.
    pub fn new(cl: &CL_HSMqk, degree: Id, some_coeffs: &BTreeMap<Id, QFI>) -> Self {
        let mut coeffs = vec![cl.one(); degree as usize + 1];
        some_coeffs
            .iter()
            .take_while(|(&id, _)| id <= degree)
            .for_each(|(&id, coeff)| coeffs[id as usize] = coeff.clone());
        Self { coeffs }
    }

    pub fn eval(&self, cl: &CL_HSMqk, x: &Zq) -> QFI {
        let mut result = cl.one();
        let x = Mpz::from(x);
        for i in (0..self.coeffs.len()).rev() {
            result = result.exp(cl, &x).compose(cl, &self.coeffs[i]);
        }
        result
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CLMultiRecvCiphertext {
    pub randomness: QFI,
    pub encryption: BTreeMap<Id, QFI>,
}

impl CLMultiRecvCiphertext {
    pub fn random(
        cl: &CL_HSMqk,
        rng: &mut RandGen,
        keyring: &CLKeyRing,
        plaintexts: &BTreeMap<Id, Zq>,
    ) -> (Self, Mpz) {
        let r = rng.random_mpz(&cl.encrypt_randomness_bound());

        let randomness = cl.power_of_h(&r);

        let encryption = plaintexts
            .iter()
            .map(|(id, m)| {
                let f_pow_m = cl.power_of_f(&Mpz::from(m));
                let pk_pow_r = keyring[id].exponentiation(cl, &r);
                (*id, f_pow_m.compose(&cl, &pk_pow_r))
            })
            .collect();

        (
            Self {
                randomness,
                encryption,
            },
            r,
        )
    }
}

type CLKeyRing = BTreeMap<Id, PublicKey>;

pub struct PubParams {
    pub cl: CL_HSMqk,
    pub t: Id, // minimal number of parties to reconstruct the secret
    // any polynomial should be of degree t-1
    pub n: Id,
    pub cl_keyring: CLKeyRing,
}

impl PubParams {
    pub fn lagrange_coeffs(&self, parties: Vec<Id>) -> Option<BTreeMap<Id, Zq>> {
        if parties.len() < self.t as usize {
            return None;
        }

        let mut coeffs = BTreeMap::new();
        for i in &parties {
            let mut num = Zq::from(1u64);
            let mut den = Zq::from(1u64);
            for j in &parties {
                if i != j {
                    num = num * Zq::from(*j as u64);
                    den = den * Zq::from(*j as i32 - *i as i32);
                }
            }
            coeffs.insert(*i, num * den.invert().unwrap());
        }
        Some(coeffs)
    }

    pub fn interpolate(&self, shares: &BTreeMap<Id, Zq>) -> Option<Zq> {
        let lagrange_coeffs = self.lagrange_coeffs(shares.keys().cloned().collect())?;
        Some(
            shares
                .iter()
                .map(|(i, share)| &lagrange_coeffs[i] * share)
                .sum(),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PvssDealing {
    pub curve_polynomial: CurvePolynomial,
    pub shares_ciphertext: CLMultiRecvCiphertext,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PvssNizk {
    pub e: Zq,
    pub z1: Mpz,
    pub z2: Zq,
}

impl PvssDealing {
    pub fn random(
        pp: &PubParams,
        rng: &mut RandGen,
        curve_generator: &G,
    ) -> (Self, Mpz, Polynomial, BTreeMap<Id, Zq>) {
        // make coefficients of a (t-1)-degree polynomial, and derive the shares
        let poly = Polynomial {
            coeffs: (0..pp.t).map(|_| Zq::random()).collect(),
        };

        let shares = (1..=pp.n)
            .into_iter()
            .map(|id| (id, poly.eval(&Zq::from(id as u64))))
            .collect();

        let curve_polynomial = CurvePolynomial::from_exp(&poly, &curve_generator);

        let (encrypted_shares, r) =
            CLMultiRecvCiphertext::random(&pp.cl, rng, &pp.cl_keyring, &shares);

        (
            Self {
                curve_polynomial,
                shares_ciphertext: encrypted_shares,
            },
            r,
            poly,
            shares,
        )
    }
}

impl PvssNizk {
    pub fn prove(
        pp: &PubParams,
        dealing: &PvssDealing,
        r: &Mpz,
        shares: &BTreeMap<Id, Zq>,
        rng: &mut RandGen,
        curve_generator: &G,
    ) -> Self {
        let u1 = rng.random_mpz(&pp.cl.encrypt_randomness_bound());
        let u2 = Zq::random();
        let U1 = &pp.cl.power_of_h(&u1);
        let U2 = curve_generator * &u2;

        let gamma = PvssNizk::challenge1(pp, dealing, curve_generator);

        let U3 = QFPolynomial::new(
            &pp.cl,
            pp.n,
            &pp.cl_keyring
                .iter()
                .map(|(&id, pk)| (id, pk.elt()))
                .collect(),
        )
        .eval(&pp.cl, &gamma)
        .exp(&pp.cl, &u1)
        .compose(&pp.cl, &pp.cl.power_of_f(&Mpz::from(&u2)));

        let e = Self::challenge2(&gamma, &U1, &U2, &U3);

        let z1 = u1 + Mpz::from(&e) * r;
        let z2 = u2 + Polynomial::new(pp.n, shares).eval(&gamma) * &e; // missing const term

        Self { e, z1, z2 }
    }

    pub fn verify(&self, dealing: &PvssDealing, pp: &PubParams, curve_generator: &G) -> bool {
        let gamma = Self::challenge1(pp, dealing, curve_generator);

        // U1
        let U1d = &dealing
            .shares_ciphertext
            .randomness
            .exp(&pp.cl, &-Mpz::from(&self.e));
        let U1 = &pp.cl.power_of_h(&self.z1).compose(&pp.cl, &U1d);

        // U2
        // curve polynomial defined by shares; O(tn), profile to decide whether to optimize
        let shares_on_curve = (1..=pp.n)
            .into_iter()
            .map(|id| (id, dealing.curve_polynomial.eval(&Zq::from(id as u64))))
            .collect();
        let shares_curve_poly = CurvePolynomial::new(pp.n, &shares_on_curve);
        let U2 = curve_generator * &self.z2 - shares_curve_poly.eval(&gamma) * &self.e;

        // U3
        let U3d = QFPolynomial::new(&pp.cl, pp.n, &dealing.shares_ciphertext.encryption)
            .eval(&pp.cl, &gamma)
            .exp(&pp.cl, &-Mpz::from(&self.e));

        let U3 = QFPolynomial::new(
            &pp.cl,
            pp.n,
            &pp.cl_keyring
                .iter()
                .map(|(&id, pk)| (id, pk.elt()))
                .collect(),
        )
        .eval(&pp.cl, &gamma)
        .exp(&pp.cl, &self.z1)
        .compose(&pp.cl, &pp.cl.power_of_f(&Mpz::from(&self.z2)))
        .compose(&pp.cl, &U3d);

        let e = Self::challenge2(&gamma, &U1, &U2, &U3);
        let result = e == self.e;
        assert!(result);
        result
    }

    fn challenge1(pp: &PubParams, pvss_dealing: &PvssDealing, curve_generator: &G) -> Zq {
        let mut hasher = Sha256::new();
        hasher.update(&pp.cl.discriminant().to_bytes());
        for (id, pk) in &pp.cl_keyring {
            hasher.update(&id.to_be_bytes());
            hasher.update(&pk.to_bytes());
        }
        hasher.update(&pvss_dealing.shares_ciphertext.randomness.to_bytes());
        for (id, enc) in &pvss_dealing.shares_ciphertext.encryption {
            hasher.update(&id.to_be_bytes());
            hasher.update(&enc.to_bytes());
        }
        hasher.update(&curve_generator.to_bytes(false));
        for coeff in &pvss_dealing.curve_polynomial.coeffs {
            hasher.update(&coeff.to_bytes(false));
        }
        Zq::from_bigint(&BigInt::from_bytes(&hasher.finalize()[..16]))
    }

    fn challenge2(gamma: &Zq, U1: &QFI, U2: &G, U3: &QFI) -> Zq {
        let hash = Sha256::new()
            .chain_update(&gamma.to_bigint().to_bytes())
            .chain_update(&U1.to_bytes())
            .chain_update(&U2.to_bytes(false))
            .chain_update(&U3.to_bytes())
            .finalize();
        Zq::from_bigint(&BigInt::from_bytes(&hash[..16]))
    }
}

/// Aggregated PVSS result
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JointPvssResult {
    pub shares_ciphertext: CLMultiRecvCiphertext,
    pub curve_polynomial: CurvePolynomial,
    pub curve_macs: BTreeMap<Id, G>,
}

impl<'a> JointPvssResult {
    pub fn new(pp: &PubParams, dealings: Vec<PvssDealing>) -> Self {
        let mut curve_coeffs = vec![G::zero(); pp.t as usize];
        for dealing in &dealings {
            for (i, coeff) in dealing.curve_polynomial.coeffs.iter().enumerate() {
                curve_coeffs[i] = &curve_coeffs[i] + coeff;
            }
        } // a tiny bit of care for cache locality

        let curve_polynomial = CurvePolynomial {
            coeffs: curve_coeffs,
        };

        let randomness = dealings
            .iter()
            .map(|d| d.shares_ciphertext.randomness.clone())
            .reduce(|acc, R| acc.compose(&pp.cl, &R))
            .unwrap()
            .clone();

        let encryption = (1..=pp.n)
            .into_iter()
            .map(|id| {
                let sum = dealings
                    .iter()
                    .map(|d| {
                        d.shares_ciphertext
                            .encryption
                            .get(&id)
                            .unwrap_or(&pp.cl.one())
                            .clone()
                    })
                    .reduce(|acc, E| acc.compose(&pp.cl, &E))
                    .unwrap()
                    .clone();
                (id, sum)
            })
            .collect();

        let curve_macs = (1..=pp.n)
            .into_iter()
            .map(|id| {
                let sum = dealings
                    .iter()
                    .map(|d| d.curve_polynomial.eval(&Zq::from(id as u64)))
                    .reduce(|acc, M| acc + M)
                    .unwrap();
                (id, sum)
            })
            .collect();

        Self {
            shares_ciphertext: CLMultiRecvCiphertext {
                randomness,
                encryption,
            },
            curve_polynomial,
            curve_macs,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MtaDealing {
    pub shares_ciphertext: CLMultiRecvCiphertext,
    pub curve_macs: BTreeMap<Id, G>,
}

impl MtaDealing {
    /// the caller should remove disqualified parties from pvss_result
    /// the pairwise shares returned should be negated when later used
    pub fn new(
        pp: &PubParams,
        pvss: &JointPvssResult,
        scalar: &Zq,
        curve_generator: &G,
    ) -> (Self, BTreeMap<Id, Zq>) {
        let randomness = pvss
            .shares_ciphertext
            .randomness
            .exp(&pp.cl, &Mpz::from(scalar));

        let multienc = &pvss.shares_ciphertext.encryption;

        let pairwise_shares: BTreeMap<Id, Zq> =
            multienc.iter().map(|(&id, _)| (id, Zq::random())).collect();

        let encryption = multienc
            .iter()
            .map(|(id, E)| {
                let res = E
                    .exp(&pp.cl, &Mpz::from(scalar))
                    .compose(&pp.cl, &pp.cl.power_of_f(&Mpz::from(&pairwise_shares[id])));
                (*id, res)
            })
            .collect();

        let curve_macs = pvss
            .curve_macs
            .iter()
            .map(|(&id, mac)| (id, scalar * mac + curve_generator * &pairwise_shares[&id]))
            .collect();

        (
            MtaDealing {
                shares_ciphertext: CLMultiRecvCiphertext {
                    randomness,
                    encryption,
                },
                curve_macs,
            },
            pairwise_shares,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MtaNizk {
    pub e: Zq,
    pub z1: Mpz,
    pub z2: Zq,
}

impl MtaNizk {
    pub fn prove(
        pp: &PubParams,
        pvss_result: &JointPvssResult,
        mta_dealing: &MtaDealing,
        curve_generator: &G,
        rng: &mut RandGen,
        scalar: &Zq,
        pairwise_shares: &BTreeMap<Id, Zq>,
    ) -> Self {
        let gamma = Self::challenge1(
            pp,
            pvss_result,
            mta_dealing,
            curve_generator,
            &(curve_generator * scalar),
        );

        let u1 = rng.random_mpz(&pp.cl.encrypt_randomness_bound());
        let u2 = Zq::random();

        let u1_modq = Zq::from(BigInt::from_bytes(&u1.to_bytes()) % Zq::group_order());
        let U1 = G::generator() * &u1_modq;
        let U2 = pvss_result.shares_ciphertext.randomness.exp(&pp.cl, &u1);

        let U3 = QFPolynomial::new(&pp.cl, pp.n, &pvss_result.shares_ciphertext.encryption)
            .eval(&pp.cl, &gamma)
            .exp(&pp.cl, &u1)
            .compose(&pp.cl, &pp.cl.power_of_f(&Mpz::from(&u2)));

        // compute original macs from pvss_result.curve_polynomial
        // TODO: profile, and may make sense to reuse what's previously computed
        let U4 = CurvePolynomial::new(pp.n, &pvss_result.curve_macs).eval(&gamma) * &u1_modq
            + curve_generator * &u2;

        let e = Self::challenge2(&gamma, &U1, &U2, &U3, &U4);
        let z1 = &u1 + Mpz::from(&(&e * scalar));
        let z2 = Polynomial::new(pp.n, pairwise_shares).eval(&gamma) * &e + &u2;

        Self { e, z1, z2 }
    }

    pub fn verify(
        &self,
        pp: &PubParams,
        pvss_result: &JointPvssResult,
        mta_dealing: &MtaDealing,
        curve_generator: &G,
        scalar_pub: &G,
    ) -> bool {
        let gamma = Self::challenge1(pp, pvss_result, mta_dealing, curve_generator, scalar_pub);

        let z1_modq = Zq::from(BigInt::from_bytes(&self.z1.to_bytes()) % Zq::group_order());
        let U1 = G::generator() * z1_modq - scalar_pub * &self.e;

        let U2 = pvss_result
            .shares_ciphertext
            .randomness
            .exp(&pp.cl, &self.z1)
            .compose(
                &pp.cl,
                &mta_dealing
                    .shares_ciphertext
                    .randomness
                    .exp(&pp.cl, &-Mpz::from(&self.e)),
            );

        // U3
        let U3d = QFPolynomial::new(&pp.cl, pp.n, &mta_dealing.shares_ciphertext.encryption)
            .eval(&pp.cl, &gamma)
            .exp(&pp.cl, &-Mpz::from(&self.e));

        let U3 = QFPolynomial::new(&pp.cl, pp.n, &pvss_result.shares_ciphertext.encryption)
            .eval(&pp.cl, &gamma)
            .exp(&pp.cl, &self.z1)
            .compose(&pp.cl, &pp.cl.power_of_f(&Mpz::from(&self.z2)))
            .compose(&pp.cl, &U3d);

        // U4
        let z1_modq = Zq::from(BigInt::from_bytes(&self.z1.to_bytes()) % Zq::group_order());

        let U4 = curve_generator * &self.z2
            + CurvePolynomial::new(pp.n, &pvss_result.curve_macs).eval(&gamma) * &z1_modq
            - CurvePolynomial::new(pp.n, &mta_dealing.curve_macs).eval(&gamma) * &self.e;

        let e = Self::challenge2(&gamma, &U1, &U2, &U3, &U4);
        let result = e == self.e;
        assert!(result);
        result
    }

    fn challenge1(
        pp: &PubParams,
        pvss_result: &JointPvssResult,
        mta_dealing: &MtaDealing,
        curve_generator: &G,
        scalar_pub: &G,
    ) -> Zq {
        let mut hasher = Sha256::new();
        hasher.update(pp.cl.discriminant().to_bytes());
        hasher.update(pvss_result.shares_ciphertext.randomness.to_bytes());
        for (id, enc) in &pvss_result.shares_ciphertext.encryption {
            hasher.update(&id.to_be_bytes());
            hasher.update(&enc.to_bytes());
        }
        hasher.update(mta_dealing.shares_ciphertext.randomness.to_bytes());
        for (id, enc) in &mta_dealing.shares_ciphertext.encryption {
            hasher.update(&id.to_be_bytes());
            hasher.update(&enc.to_bytes());
        }
        for (id, mac) in &pvss_result.curve_macs {
            hasher.update(&id.to_be_bytes());
            hasher.update(&mac.to_bytes(false));
        }
        for (id, mac) in &mta_dealing.curve_macs {
            hasher.update(&id.to_be_bytes());
            hasher.update(&mac.to_bytes(false));
        }
        hasher.update(curve_generator.to_bytes(true));
        hasher.update(scalar_pub.to_bytes(true));

        Zq::from_bigint(&BigInt::from_bytes(&hasher.finalize()[..16]))
    }

    fn challenge2(gamma: &Zq, U1: &G, U2: &QFI, U3: &QFI, U4: &G) -> Zq {
        let hash = Sha256::new()
            .chain_update(&gamma.to_bigint().to_bytes())
            .chain_update(&U1.to_bytes(false))
            .chain_update(&U2.to_bytes())
            .chain_update(&U3.to_bytes())
            .chain_update(&U4.to_bytes(false))
            .finalize();
        Zq::from_bigint(&BigInt::from_bytes(&hash[..16]))

    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DleqNizk {
    pub e: Zq,
    pub z: Zq,
}

impl DleqNizk {
    pub fn prove(gen1: &G, pow1: &G, gen2: &G, pow2: &G, x: &Zq) -> Self {
        let u = Zq::random();
        let U1 = gen1 * &u;
        let U2 = gen2 * &u;
        let e = Self::challenge(gen1, pow1, gen2, pow2, &U1, &U2);
        let z = &u + &e * x;
        Self { e, z }
    }

    pub fn verify(&self, gen1: &G, pow1: &G, gen2: &G, pow2: &G) -> bool {
        let U1 = gen1 * &self.z - pow1 * &self.e;
        let U2 = gen2 * &self.z - pow2 * &self.e;
        let e = Self::challenge(gen1, pow1, gen2, pow2, &U1, &U2);

        let result = e == self.e;
        assert!(result);
        result
    }

    fn challenge(gen1: &G, pow1: &G, gen2: &G, pow2: &G, U1: &G, U2: &G) -> Zq {
        let mut hasher = Sha256::new();
        for point in &[gen1, pow1, gen2, pow2, U1, U2] {
            hasher.update(point.to_bytes(false));
        }
        Zq::from_bigint(&BigInt::from_bytes(&hasher.finalize()[..16]))
    }
}
