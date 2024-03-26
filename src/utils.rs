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
use std::ops::{Add, Mul};
use std::{collections::BTreeMap, ops::Deref};
use thiserror::Error;

use rayon::prelude::*;

use crate::lagrange_coeff;

type Zq = Scalar<Secp256k1>;
type G = Point<Secp256k1>;

type Id = u8;

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
    pub fn new(
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
    pub fn new(
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
            CLMultiRecvCiphertext::new(&pp.cl, rng, &pp.cl_keyring, &shares);

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
        dealing: &PvssDealing,
        r: &Mpz,
        shares: &BTreeMap<Id, Zq>,
        pp: &PubParams,
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
            .exp(&pp.cl, &Mpz::from(&-&self.e));
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
            .exp(&pp.cl, &Mpz::from(&-&self.e));

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
        self.e == e
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
        Zq::from_bytes(&hasher.finalize()[..16]).unwrap()
    }

    fn challenge2(gamma: &Zq, U1: &QFI, U2: &G, U3: &QFI) -> Zq {
        let hash = Sha256::new()
            .chain_update(&gamma.to_bigint().to_bytes())
            .chain_update(&U1.to_bytes())
            .chain_update(&U2.to_bytes(false))
            .chain_update(&U3.to_bytes())
            .finalize();
        Zq::from_bytes(&hash[..16]).unwrap()
    }
}

/// Aggregated PVSS result
pub struct JointPvssResult {
    pub shares_ciphertext: CLMultiRecvCiphertext,
    pub curve_polynomial: CurvePolynomial,
    pub curve_macs: BTreeMap<Id, G>,
}

impl JointPvssResult {
    pub fn new(pp: &PubParams, dealings: &[PvssDealing]) -> Self {
        let mut curve_coeffs = vec![G::zero(); pp.t as usize];
        for dealing in dealings {
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
                    .exp(&pp.cl, &Mpz::from(&-&self.e)),
            );

        // U3
        let U3d = QFPolynomial::new(&pp.cl, pp.n, &mta_dealing.shares_ciphertext.encryption)
            .eval(&pp.cl, &gamma)
            .exp(&pp.cl, &Mpz::from(&-&self.e));

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
        e == self.e
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

        Zq::from_bytes(&hasher.finalize()[..16]).unwrap()
    }

    fn challenge2(gamma: &Zq, U1: &G, U2: &QFI, U3: &QFI, U4: &G) -> Zq {
        let hash = Sha256::new()
            .chain_update(&gamma.to_bigint().to_bytes())
            .chain_update(&U1.to_bytes(false))
            .chain_update(&U2.to_bytes())
            .chain_update(&U3.to_bytes())
            .chain_update(&U4.to_bytes(false))
            .finalize();
        Zq::from_bytes(&hash[..16]).unwrap()
    }
}

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
        e == self.e
    }

    fn challenge(gen1: &G, pow1: &G, gen2: &G, pow2: &G, U1: &G, U2: &G) -> Zq {
        let mut hasher = Sha256::new();
        for point in &[gen1, pow1, gen2, pow2, U1, U2] {
            hasher.update(point.to_bytes(false));
        }
        Zq::from_bytes(&hasher.finalize()[..16]).unwrap()
    }
}


// #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
// pub struct NiDkgOutput {
//     pub parties: Vec<usize>, // ids of parties, used as indexes of all hashmaps
//     pub share: Scalar<Secp256k1>,
//     pub pk: Point<Secp256k1>,
//     pub shares_cmt: BTreeMap<usize, Point<Secp256k1>>,
//     pub encrypted_shares: Option<BTreeMap<usize, CipherText>>,
// }
// impl NiDkgOutput {
//     pub fn from_combining(
//         parties: Vec<usize>,
//         messages: &[PvssDealing],
//         myid: usize,
//         clgroup: CL_HSMqk,
//         rand_gen: &mut RandGen,
//         want_encrypted_shares: bool,
//         clpk: BTreeMap<usize, PublicKey>,
//         mysk: &SecretKey,
//     ) -> Self {
//         let honest_parties: Vec<usize> = parties
//             .into_iter()
//             .filter(|j| PvssNizk::verify(&messages[*j], &clgroup, &clpk))
//             .collect();

//         let mut x_i = Scalar::<Secp256k1>::from(0);
//         let mut X = Point::<Secp256k1>::zero();
//         let mut X_j_list = BTreeMap::<usize, Point<Secp256k1>>::new();

//         for &j in &honest_parties {
//             let ct = CipherText::new(&messages[j].rand_cmt, &messages[j].encrypted_shares[&myid]);
//             let pt = clgroup.decrypt(mysk, &ct);
//             x_i = x_i
//                 + Scalar::<Secp256k1>::from_bigint(&BigInt::from_bytes(
//                     pt.mpz().to_bytes().as_slice(),
//                 ));

//             X = X + &messages[j].poly_coeff_cmt[0];

//             // additively make the committed shares
//             for &l in &honest_parties {
//                 let addition = messages[j]
//                     .poly_coeff_cmt
//                     .iter()
//                     .enumerate()
//                     .map(|(k, A)| {
//                         A * Scalar::<Secp256k1>::from((l + 1).pow(k.try_into().unwrap()) as u64)
//                     })
//                     .sum::<Point<Secp256k1>>();
//                 let new_X_l = &*X_j_list.entry(l).or_insert(Point::<Secp256k1>::zero()) + addition;
//                 X_j_list.insert(l, new_X_l);
//             }
//         }

//         let mut c_j_list = BTreeMap::<usize, CipherText>::new();

//         // combine ciphertexts of shares which is expensive and therefore optional
//         if want_encrypted_shares {
//             for j in &honest_parties {
//                 let c_j = honest_parties
//                     .iter()
//                     .map(|&l| {
//                         CipherText::new(&messages[l].rand_cmt, &messages[l].encrypted_shares[j])
//                     })
//                     .reduce(|sum, ct| clgroup.add_ciphertexts(&clpk[j], &sum, &ct, rand_gen))
//                     .unwrap();
//                 c_j_list.insert(*j, c_j.clone());
//             }
//         }

//         NiDkgOutput {
//             parties: honest_parties,
//             share: x_i,
//             pk: X,
//             shares_cmt: X_j_list,
//             encrypted_shares: match want_encrypted_shares {
//                 true => Some(c_j_list),
//                 false => None,
//             },
//         }
//     }
// }

// below are code for testing

#[derive(Clone, Debug, PartialEq, ProtocolMessage, Serialize, Deserialize)]
pub enum Msg {
    PvssMsg((PvssDealing, PvssNizk)),
}

pub async fn protocol_ni_dkg<M>(
    party: M,
    myid: PartyIndex,
    t: usize,
    n: usize,
    clgroup: CL_HSMqk,
    mut rand_gen: RandGen,
    clpk: BTreeMap<usize, PublicKey>,
    mysk: SecretKey,
) -> Result<NiDkgOutput, Error<M::ReceiveError, M::SendError>>
where
    M: Mpc<ProtocolMessage = Msg>,
{
    let MpcParty { delivery, .. } = party.into_party();
    let (incoming, mut outgoing) = delivery.split();
    let mut rounds = RoundsRouter::<Msg>::builder();
    let round1 = rounds.add_round(RoundInput::<PvssDealing>::broadcast(
        myid,
        n.try_into().unwrap(),
    ));
    let mut rounds = rounds.listen(incoming);

    let my_ni_dkg_msg = PvssDealing::new(t, (0..n).collect(), &clgroup, &mut rand_gen, &clpk);

    outgoing
        .send(Outgoing::broadcast(Msg::PvssMsg(my_ni_dkg_msg.clone())))
        .await
        .unwrap();

    let all_messages = rounds
        .complete(round1)
        .await
        .unwrap()
        .into_vec_including_me(my_ni_dkg_msg);

    Ok(NiDkgOutput::from_combining(
        (0..n).collect(),
        &all_messages,
        myid.into(),
        clgroup,
        &mut rand_gen,
        false,
        clpk,
        &mysk,
    ))
}

#[derive(Debug, Error)]
pub enum Error<RecvErr, SendErr> {
    Round1Send(SendErr),
    Round1Receive(RecvErr),
}

#[tokio::test]
async fn test_cl_keygen_overhead() {
    let n: u16 = 6;

    let seed = Mpz::from(chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default());
    let mut rand_gen = RandGen::new();
    rand_gen.set_seed(&seed);

    let clgroup =
        CL_HSMqk::with_qnbits_rand_gen(50, 1, 150, &mut rand_gen, &Mpz::from(0i64), false);

    let mut clsk = BTreeMap::<usize, SecretKey>::new();
    let mut clpk = BTreeMap::<usize, PublicKey>::new();

    for i in 0..n {
        let sk_i = clgroup.secret_key_gen(&mut rand_gen);
        let pk_i = clgroup.public_key_gen(&sk_i);
        clsk.insert(i.into(), sk_i);
        clpk.insert(i.into(), pk_i);
    }
}

#[tokio::test]
async fn test_ni_dkg() {
    let n: u16 = 3;
    let t: usize = 2;

    let mut simulation = Simulation::<Msg>::new();
    let mut party_output = vec![];

    let seed = Mpz::from(chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default());
    let mut rand_gen = RandGen::new();
    rand_gen.set_seed(&seed);

    let clgroup =
        CL_HSMqk::with_qnbits_rand_gen(50, 1, 150, &mut rand_gen, &Mpz::from(0i64), false);

    let mut clsk = BTreeMap::<usize, SecretKey>::new();
    let mut clpk = BTreeMap::<usize, PublicKey>::new();

    for i in 0..n {
        let sk_i = clgroup.secret_key_gen(&mut rand_gen);
        let pk_i = clgroup.public_key_gen(&sk_i);
        clsk.insert(i.into(), sk_i);
        clpk.insert(i.into(), pk_i);
    }

    for i in 0..n {
        let party = simulation.add_party();
        let mysk = clsk[&(i as usize)].clone();

        let mut rand = RandGen::new();
        rand.set_seed(&rand_gen.random_mpz(&clgroup.encrypt_randomness_bound()));

        let output = protocol_ni_dkg(
            party,
            i,
            t,
            n.into(),
            clgroup.clone(),
            rand,
            clpk.clone(),
            mysk,
        );
        party_output.push(output);
    }

    let _output = futures::future::try_join_all(party_output).await.unwrap();
}
