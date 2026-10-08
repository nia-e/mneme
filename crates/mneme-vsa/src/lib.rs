//! Deterministic Fourier Holographic Reduced Representations (FHRR).
//!
//! This crate is an isolated capacity experiment, not mneme's canonical storage.
//! A hologram is useful only together with a cleanup dictionary (or the compute
//! needed to regenerate it), so callers should account for both. The `study`
//! binary does exactly that.
//!
//! The representation uses independent unit-complex atomic coordinates. Binding
//! is element-wise complex multiplication, bundling is a scaled sum, unbinding
//! multiplies by the role's conjugate, and cleanup ranks a bounded dictionary by
//! real cosine similarity over the flattened real/imaginary coordinates.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Current serialization/compatibility version for [`Fingerprint`].
pub const FINGERPRINT_VERSION: u16 = 1;
/// Algebra identifier. Changing binding, inverse, or bundle scaling requires a
/// new identifier even if dimensions happen to match.
pub const OPERATOR_ID: &str = "fhrr-complex-multiply-conjugate-sqrt-bundle-v1";
/// Arithmetic identity for [`MutableBundle`]. This is deliberately separate
/// from [`OPERATOR_ID`]: a mutable `f32` running sum is not bitwise equivalent
/// to [`Fhrr::superpose`], which accumulates in `f64` before casting.
pub const MUTABLE_BUNDLE_OPERATOR_ID: &str =
    "fhrr-complex-multiply-conjugate-sqrt-bundle-f32-incremental-v1";
/// Deterministic atom-codebook generator identifier.
pub const ATOM_GENERATOR_ID: &str = "fnv1a64-fmix64-splitmix64-marsaglia-unit-circle-f32-v1";
/// Practical per-vector dimension ceiling for this experimental crate.
///
/// One vector at the ceiling has an 8 MiB coordinate payload. Study binaries
/// impose stricter whole-workload limits before constructing codebooks.
pub const MAX_DIMENSION: usize = 1_048_576;

/// Full compatibility identity for a vector/codebook family.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub version: u16,
    pub dimension: usize,
    pub seed: u64,
    pub operator: String,
    pub atom_generator: String,
}

impl Fingerprint {
    pub fn new(dimension: usize, seed: u64) -> Result<Self, Error> {
        validate_dimension(dimension)?;
        Ok(Self {
            version: FINGERPRINT_VERSION,
            dimension,
            seed,
            operator: OPERATOR_ID.into(),
            atom_generator: ATOM_GENERATOR_ID.into(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Complex32 {
    pub re: f32,
    pub im: f32,
}

impl Complex32 {
    fn multiply(self, rhs: Self) -> Self {
        Self {
            // Keep the two products explicit so swapping operands is bitwise
            // commutative as well as mathematically commutative.
            re: self.re * rhs.re - self.im * rhs.im,
            im: self.re * rhs.im + self.im * rhs.re,
        }
    }

    fn conjugate(self) -> Self {
        Self {
            re: self.re,
            im: -self.im,
        }
    }
}

/// One FHRR vector tied to an exact codebook/algebra fingerprint.
#[derive(Clone)]
pub struct Vector {
    coordinates: Vec<Complex32>,
    fingerprint: Arc<Fingerprint>,
}

impl Vector {
    pub fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
    }

    pub fn dimension(&self) -> usize {
        self.coordinates.len()
    }

    /// Raw coordinate payload only: two `f32`s per dimension. This deliberately
    /// excludes `Vec`/`Arc` metadata and every role/filler cleanup vector.
    pub fn payload_bytes(&self) -> usize {
        checked_coordinate_payload_bytes(self.dimension())
            .expect("all Vector dimensions are validated at construction")
    }

    pub fn coordinates(&self) -> &[Complex32] {
        &self.coordinates
    }

    fn ensure_finite(&self) -> Result<(), Error> {
        ensure_finite_coordinates(&self.coordinates)
    }
}

impl fmt::Debug for Vector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vector")
            .field("dimension", &self.dimension())
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl PartialEq for Vector {
    fn eq(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint && self.coordinates == other.coordinates
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CleanupHit<'a> {
    pub label: &'a str,
    pub score: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    ZeroDimension,
    DimensionTooLarge {
        max: usize,
    },
    EmptySuperposition,
    EmptyBundle,
    BundleCountUnderflow,
    BundleCountOverflow,
    ZeroNorm,
    NonFiniteState,
    UnsupportedFingerprint(Box<Fingerprint>),
    FingerprintMismatch {
        expected: Box<Fingerprint>,
        found: Box<Fingerprint>,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroDimension => f.write_str("FHRR dimension must be greater than zero"),
            Self::DimensionTooLarge { max } => {
                write!(f, "FHRR dimension exceeds the supported maximum of {max}")
            }
            Self::EmptySuperposition => f.write_str("cannot superpose an empty vector set"),
            Self::EmptyBundle => f.write_str("cannot estimate an empty mutable FHRR bundle"),
            Self::BundleCountUnderflow => {
                f.write_str("cannot subtract a term from an empty mutable FHRR bundle")
            }
            Self::BundleCountOverflow => f.write_str("mutable FHRR bundle term count overflow"),
            Self::ZeroNorm => f.write_str("cosine similarity is undefined for a zero-norm vector"),
            Self::NonFiniteState => f.write_str("FHRR state contains a non-finite coordinate"),
            Self::UnsupportedFingerprint(fp) => {
                write!(f, "unsupported FHRR fingerprint: {fp:?}")
            }
            Self::FingerprintMismatch { expected, found } => write!(
                f,
                "incompatible FHRR vectors: expected {expected:?}, found {found:?}"
            ),
        }
    }
}

/// A mutable, rebuildable derived FHRR relation bundle.
///
/// This type intentionally stores only an `f32` coordinate sum and a term
/// count. It does **not** track members or establish deletion semantics.
/// Callers must retain canonical relation state and may call
/// [`Self::subtract_known_term`] only for a compatible bound term they have
/// independently proven is present. That canonical provenance is also what
/// makes a safe rebuild possible.
#[derive(Clone, Debug, PartialEq)]
pub struct MutableBundle {
    coordinates: Vec<Complex32>,
    fingerprint: Arc<Fingerprint>,
    term_count: usize,
}

/// A hologram estimated through [`MutableBundle`]'s incremental `f32`
/// accumulator. It deliberately has a distinct type and does not dereference
/// to [`Vector`]: a mutable estimate is not a clean `f64`
/// [`Fhrr::superpose`] result, and algebra which consumes it must preserve that
/// provenance explicitly.
#[derive(Clone, Debug, PartialEq)]
pub struct MutableEstimate {
    vector: Vector,
}

impl MutableEstimate {
    /// Versioned identity of the arithmetic which produced this estimate.
    pub const fn algorithm_id(&self) -> &'static str {
        MUTABLE_BUNDLE_OPERATOR_ID
    }

    /// The clean codebook compatibility identity. Pair it with
    /// [`Self::algorithm_id`] when recording provenance.
    pub fn fingerprint(&self) -> &Fingerprint {
        self.vector.fingerprint()
    }

    /// Explicitly discard the mutable arithmetic provenance.
    ///
    /// Prefer [`Fhrr::unbind_mutable`], [`Fhrr::cosine_mutable`], and
    /// [`Fhrr::cleanup_mutable`] while evaluating an incremental estimate.
    pub fn into_untyped_vector_discarding_provenance(self) -> Vector {
        self.vector
    }
}

impl MutableBundle {
    /// Start an empty bundle for this exact FHRR codebook family.
    pub fn new(fhrr: &Fhrr) -> Self {
        Self {
            coordinates: vec![Complex32 { re: 0.0, im: 0.0 }; fhrr.fingerprint.dimension],
            fingerprint: fhrr.fingerprint.clone(),
            term_count: 0,
        }
    }

    /// Versioned identifier for the mutable `f32` accumulation algorithm.
    pub const fn algorithm_id(&self) -> &'static str {
        MUTABLE_BUNDLE_OPERATOR_ID
    }

    pub fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
    }

    pub fn term_count(&self) -> usize {
        self.term_count
    }

    /// Raw coordinate payload only: two `f32`s per dimension. This is not a
    /// resident-memory estimate and excludes allocation metadata.
    pub fn coordinate_payload_bytes(&self) -> usize {
        checked_coordinate_payload_bytes(self.coordinates.len())
            .expect("all MutableBundle dimensions are validated at construction")
    }

    /// Fixed serialized count metadata for the experiment's `usize` counter.
    /// It excludes container and allocator metadata.
    pub const fn count_metadata_bytes(&self) -> usize {
        std::mem::size_of::<usize>()
    }

    /// Add one compatible, already-bound role/filler term.
    pub fn add_bound_term(&mut self, term: &Vector) -> Result<(), Error> {
        self.ensure_compatible(term)?;
        let next_count = self
            .term_count
            .checked_add(1)
            .ok_or(Error::BundleCountOverflow)?;
        let next = self.updated_coordinates(term, 1.0)?;
        self.coordinates = next;
        self.term_count = next_count;
        Ok(())
    }

    /// Subtract one compatible **caller-proven present** bound term.
    ///
    /// This is not membership-aware deletion. The accumulator retains no term
    /// set, so canonical adjacency/provenance must prove that `term` is present
    /// before this method is called.
    pub fn subtract_known_term(&mut self, term: &Vector) -> Result<(), Error> {
        self.ensure_compatible(term)?;
        let next_count = self
            .term_count
            .checked_sub(1)
            .ok_or(Error::BundleCountUnderflow)?;
        let next = self.updated_coordinates(term, -1.0)?;
        self.coordinates = next;
        self.term_count = next_count;
        Ok(())
    }

    /// Atomically replace one **caller-proven present** bound term.
    ///
    /// Like [`Self::subtract_known_term`], this does not prove membership. The
    /// caller must establish that `old_term` is present and that `new_term` is
    /// the canonical replacement. Both compatibility checks and the complete
    /// coordinate update finish before this bundle is mutated, so a failed
    /// replacement cannot leave the old term subtracted.
    pub fn replace_known_term(
        &mut self,
        old_term: &Vector,
        new_term: &Vector,
    ) -> Result<(), Error> {
        self.ensure_compatible(old_term)?;
        self.ensure_compatible(new_term)?;
        self.term_count
            .checked_sub(1)
            .ok_or(Error::BundleCountUnderflow)?;
        let next: Vec<_> = self
            .coordinates
            .iter()
            .zip(&old_term.coordinates)
            .zip(&new_term.coordinates)
            .map(|((sum, old), new)| Complex32 {
                re: (sum.re - old.re) + new.re,
                im: (sum.im - old.im) + new.im,
            })
            .collect();
        ensure_finite_coordinates(&next)?;
        self.coordinates = next;
        Ok(())
    }

    /// Estimate the scaled hologram. Empty bundles deliberately abstain.
    pub fn estimate(&self) -> Result<MutableEstimate, Error> {
        if self.term_count == 0 {
            return Err(Error::EmptyBundle);
        }
        ensure_finite_coordinates(&self.coordinates)?;
        let scale = 1.0_f32 / (self.term_count as f32).sqrt();
        if !scale.is_finite() {
            return Err(Error::NonFiniteState);
        }
        let coordinates: Vec<_> = self
            .coordinates
            .iter()
            .map(|coordinate| Complex32 {
                re: coordinate.re * scale,
                im: coordinate.im * scale,
            })
            .collect();
        if !coordinates
            .iter()
            .all(|coordinate| coordinate.re.is_finite() && coordinate.im.is_finite())
        {
            return Err(Error::NonFiniteState);
        }
        let norm_squared = coordinates.iter().fold(0.0_f64, |sum, coordinate| {
            sum + f64::from(coordinate.re).mul_add(
                f64::from(coordinate.re),
                f64::from(coordinate.im) * f64::from(coordinate.im),
            )
        });
        if !norm_squared.is_finite() {
            return Err(Error::NonFiniteState);
        }
        if norm_squared == 0.0 {
            return Err(Error::ZeroNorm);
        }
        Ok(MutableEstimate {
            vector: Vector {
                coordinates,
                fingerprint: self.fingerprint.clone(),
            },
        })
    }

    fn ensure_compatible(&self, term: &Vector) -> Result<(), Error> {
        ensure_finite_coordinates(&self.coordinates)?;
        term.ensure_finite()?;
        if self.fingerprint.as_ref() == term.fingerprint.as_ref() {
            Ok(())
        } else {
            Err(Error::FingerprintMismatch {
                expected: Box::new(self.fingerprint.as_ref().clone()),
                found: Box::new(term.fingerprint.as_ref().clone()),
            })
        }
    }

    fn updated_coordinates(&self, term: &Vector, sign: f32) -> Result<Vec<Complex32>, Error> {
        ensure_finite_coordinates(&self.coordinates)?;
        term.ensure_finite()?;
        let next: Vec<_> = self
            .coordinates
            .iter()
            .zip(&term.coordinates)
            .map(|(sum, term)| Complex32 {
                re: sum.re + sign * term.re,
                im: sum.im + sign * term.im,
            })
            .collect();
        if next
            .iter()
            .all(|coordinate| coordinate.re.is_finite() && coordinate.im.is_finite())
        {
            Ok(next)
        } else {
            Err(Error::NonFiniteState)
        }
    }
}

impl std::error::Error for Error {}

/// Deterministic FHRR algebra and atom-codebook generator.
#[derive(Clone, Debug)]
pub struct Fhrr {
    fingerprint: Arc<Fingerprint>,
}

impl Fhrr {
    pub fn new(dimension: usize, seed: u64) -> Result<Self, Error> {
        Self::from_fingerprint(Fingerprint::new(dimension, seed)?)
    }

    pub fn from_fingerprint(fingerprint: Fingerprint) -> Result<Self, Error> {
        validate_dimension(fingerprint.dimension)?;
        if fingerprint.version != FINGERPRINT_VERSION
            || fingerprint.operator != OPERATOR_ID
            || fingerprint.atom_generator != ATOM_GENERATOR_ID
        {
            return Err(Error::UnsupportedFingerprint(Box::new(fingerprint)));
        }
        Ok(Self {
            fingerprint: Arc::new(fingerprint),
        })
    }

    pub fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
    }

    /// Raw bytes for one hologram coordinate payload. This is not total system
    /// memory; cleanup dictionaries and labels are additional.
    pub fn payload_bytes(&self) -> usize {
        checked_coordinate_payload_bytes(self.fingerprint.dimension)
            .expect("Fhrr fingerprint dimensions are validated at construction")
    }

    /// Generate an order-independent deterministic atomic vector for `label`.
    /// Every coordinate has magnitude one up to `f32` rounding.
    pub fn atom(&self, label: &str) -> Vector {
        let mut state = seeded_label_hash(self.fingerprint.seed, label.as_bytes());
        let mut coordinates = Vec::with_capacity(self.fingerprint.dimension);
        while coordinates.len() < self.fingerprint.dimension {
            // Rejection-sample a point uniformly from the unit disk, then
            // normalize it onto the circle. Its angle is uniform, and unlike a
            // sin/cos phase conversion this uses only deterministic IEEE-754
            // arithmetic (including square root), not platform libm trig.
            let re = signed_unit(splitmix64(&mut state));
            let im = signed_unit(splitmix64(&mut state));
            let norm_squared = re.mul_add(re, im * im);
            if norm_squared > 0.0 && norm_squared <= 1.0 {
                let inverse_norm = norm_squared.sqrt().recip();
                coordinates.push(Complex32 {
                    re: (re * inverse_norm) as f32,
                    im: (im * inverse_norm) as f32,
                });
            }
        }
        Vector {
            coordinates,
            fingerprint: self.fingerprint.clone(),
        }
    }

    /// Element-wise complex multiplication. FHRR binding is commutative; role
    /// direction comes from which atom is conjugated during unbinding.
    pub fn bind(&self, left: &Vector, right: &Vector) -> Result<Vector, Error> {
        self.ensure_compatible(left)?;
        self.ensure_compatible(right)?;
        let coordinates: Vec<_> = left
            .coordinates
            .iter()
            .zip(&right.coordinates)
            .map(|(&a, &b)| a.multiply(b))
            .collect();
        ensure_finite_coordinates(&coordinates)?;
        Ok(Vector {
            coordinates,
            fingerprint: self.fingerprint.clone(),
        })
    }

    pub fn conjugate(&self, vector: &Vector) -> Result<Vector, Error> {
        self.ensure_compatible(vector)?;
        let coordinates: Vec<_> = vector
            .coordinates
            .iter()
            .copied()
            .map(Complex32::conjugate)
            .collect();
        ensure_finite_coordinates(&coordinates)?;
        Ok(Vector {
            coordinates,
            fingerprint: self.fingerprint.clone(),
        })
    }

    /// Recover the filler estimate from `hologram` by binding it with the
    /// conjugate (multiplicative inverse) of `role`.
    pub fn unbind(&self, hologram: &Vector, role: &Vector) -> Result<Vector, Error> {
        let inverse = self.conjugate(role)?;
        self.bind(hologram, &inverse)
    }

    /// Unbind an incremental estimate without erasing its arithmetic origin.
    pub fn unbind_mutable(
        &self,
        hologram: &MutableEstimate,
        role: &Vector,
    ) -> Result<MutableEstimate, Error> {
        let inverse = self.conjugate(role)?;
        let vector = self.bind(&hologram.vector, &inverse)?;
        Ok(MutableEstimate { vector })
    }

    /// Bundle vectors by `1/sqrt(n)`-scaled component-wise addition. The scale
    /// keeps expected energy stable as pair count grows; cosine cleanup itself is
    /// invariant to this global scale.
    pub fn superpose(&self, vectors: &[Vector]) -> Result<Vector, Error> {
        let Some(first) = vectors.first() else {
            return Err(Error::EmptySuperposition);
        };
        self.ensure_compatible(first)?;
        let mut sums = vec![(0.0_f64, 0.0_f64); self.fingerprint.dimension];
        for vector in vectors {
            self.ensure_compatible(vector)?;
            for (sum, coordinate) in sums.iter_mut().zip(&vector.coordinates) {
                sum.0 += f64::from(coordinate.re);
                sum.1 += f64::from(coordinate.im);
                if !sum.0.is_finite() || !sum.1.is_finite() {
                    return Err(Error::NonFiniteState);
                }
            }
        }
        let scale = 1.0 / (vectors.len() as f64).sqrt();
        if !scale.is_finite() {
            return Err(Error::NonFiniteState);
        }
        let coordinates: Vec<_> = sums
            .into_iter()
            .map(|(re, im)| Complex32 {
                re: (re * scale) as f32,
                im: (im * scale) as f32,
            })
            .collect();
        ensure_finite_coordinates(&coordinates)?;
        Ok(Vector {
            coordinates,
            fingerprint: self.fingerprint.clone(),
        })
    }

    /// Real cosine over complex coordinates, equivalently
    /// `Re(sum(conj(left) * right)) / (||left|| ||right||)`.
    pub fn cosine(&self, left: &Vector, right: &Vector) -> Result<f32, Error> {
        self.ensure_compatible(left)?;
        self.ensure_compatible(right)?;
        let mut dot = 0.0_f64;
        let mut left_norm = 0.0_f64;
        let mut right_norm = 0.0_f64;
        for (a, b) in left.coordinates.iter().zip(&right.coordinates) {
            let ar = f64::from(a.re);
            let ai = f64::from(a.im);
            let br = f64::from(b.re);
            let bi = f64::from(b.im);
            dot += ar.mul_add(br, ai * bi);
            left_norm += ar.mul_add(ar, ai * ai);
            right_norm += br.mul_add(br, bi * bi);
            if !dot.is_finite() || !left_norm.is_finite() || !right_norm.is_finite() {
                return Err(Error::NonFiniteState);
            }
        }
        if left_norm == 0.0 || right_norm == 0.0 {
            return Err(Error::ZeroNorm);
        }
        let denominator = left_norm.sqrt() * right_norm.sqrt();
        let score = dot / denominator;
        if !denominator.is_finite() || !score.is_finite() || !(score as f32).is_finite() {
            return Err(Error::NonFiniteState);
        }
        Ok(score as f32)
    }

    /// Compare a clean vector with an incremental estimate while retaining the
    /// latter's distinct provenance at the API boundary.
    pub fn cosine_mutable(
        &self,
        clean: &Vector,
        incremental: &MutableEstimate,
    ) -> Result<f32, Error> {
        self.cosine(clean, &incremental.vector)
    }

    /// Exhaustively rank a supplied cleanup dictionary. Dictionary construction,
    /// storage, and this `O(dictionary * dimension)` scan are deliberately not
    /// hidden by the API.
    pub fn cleanup<'a, I>(
        &self,
        query: &Vector,
        candidates: I,
        k: usize,
    ) -> Result<Vec<CleanupHit<'a>>, Error>
    where
        I: IntoIterator<Item = (&'a str, &'a Vector)>,
    {
        self.ensure_compatible(query)?;
        // With no requested hits the candidate iterator is intentionally not
        // consumed or validated; there is no cleanup work to perform.
        if k == 0 {
            return Ok(Vec::new());
        }
        let mut hits = candidates
            .into_iter()
            .map(|(label, vector)| {
                Ok(CleanupHit {
                    label,
                    score: self.cosine(query, vector)?,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.label.cmp(b.label))
        });
        hits.truncate(k.min(hits.len()));
        Ok(hits)
    }

    /// Exhaustively clean up an incremental estimate without first converting
    /// it to an untagged [`Vector`].
    pub fn cleanup_mutable<'a, I>(
        &self,
        query: &MutableEstimate,
        candidates: I,
        k: usize,
    ) -> Result<Vec<CleanupHit<'a>>, Error>
    where
        I: IntoIterator<Item = (&'a str, &'a Vector)>,
    {
        self.cleanup(&query.vector, candidates, k)
    }

    fn ensure_compatible(&self, vector: &Vector) -> Result<(), Error> {
        vector.ensure_finite()?;
        if self.fingerprint.as_ref() == vector.fingerprint.as_ref() {
            Ok(())
        } else {
            Err(Error::FingerprintMismatch {
                expected: Box::new(self.fingerprint.as_ref().clone()),
                found: Box::new(vector.fingerprint.as_ref().clone()),
            })
        }
    }
}

fn validate_dimension(dimension: usize) -> Result<(), Error> {
    if dimension == 0 {
        return Err(Error::ZeroDimension);
    }
    if dimension > MAX_DIMENSION {
        return Err(Error::DimensionTooLarge { max: MAX_DIMENSION });
    }
    Ok(())
}

fn checked_coordinate_payload_bytes(dimension: usize) -> Option<usize> {
    dimension
        .checked_mul(2)?
        .checked_mul(std::mem::size_of::<f32>())
}

fn ensure_finite_coordinates(coordinates: &[Complex32]) -> Result<(), Error> {
    if coordinates
        .iter()
        .all(|coordinate| coordinate.re.is_finite() && coordinate.im.is_finite())
    {
        Ok(())
    } else {
        Err(Error::NonFiniteState)
    }
}

fn seeded_label_hash(seed: u64, label: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ seed.rotate_left(17);
    for &byte in label {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // MurmurHash3's 64-bit finalizer prevents nearby labels/seeds from entering
    // SplitMix with nearby states. This is deterministic, not cryptographic.
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^ (hash >> 33)
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn signed_unit(random: u64) -> f64 {
    let unit = ((random >> 11) as f64) * (1.0 / ((1u64 << 53) as f64));
    unit.mul_add(2.0, -1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn non_finite_vector(fhrr: &Fhrr) -> Vector {
        Vector {
            coordinates: vec![
                Complex32 {
                    re: f32::NAN,
                    im: 0.0
                };
                fhrr.fingerprint.dimension
            ],
            fingerprint: fhrr.fingerprint.clone(),
        }
    }

    /// This intentionally does not use `MutableBundle`'s update helper: it is
    /// the coordinate-level reference for the incremental `f32` accumulator.
    fn coordinate_accumulator_oracle(terms: &[(f32, &Vector)]) -> Vec<Complex32> {
        let mut coordinates = vec![Complex32 { re: 0.0, im: 0.0 }; terms[0].1.dimension()];
        for &(sign, term) in terms {
            for (sum, coordinate) in coordinates.iter_mut().zip(term.coordinates()) {
                sum.re += sign * coordinate.re;
                sum.im += sign * coordinate.im;
            }
        }
        coordinates
    }

    #[test]
    fn atoms_are_deterministic_unit_complex_vectors() {
        let fhrr = Fhrr::new(512, 7).unwrap();
        let first = fhrr.atom("role:subject");
        let second = fhrr.atom("role:subject");
        assert_eq!(
            first
                .coordinates()
                .iter()
                .take(4)
                .map(|c| (c.re.to_bits(), c.im.to_bits()))
                .collect::<Vec<_>>(),
            [
                (3_205_703_376, 3_209_788_212),
                (1_065_304_568, 1_033_623_887),
                (3_194_270_797, 1_064_929_418),
                (1_055_383_557, 3_211_017_727),
            ],
            "changing the atom stream requires a new ATOM_GENERATOR_ID"
        );
        assert_eq!(first, second);
        for coordinate in first.coordinates() {
            let magnitude = coordinate.re.hypot(coordinate.im);
            assert!((magnitude - 1.0).abs() < 1.0e-6);
        }
    }

    #[test]
    fn bind_then_conjugate_unbind_recovers_the_filler() {
        let fhrr = Fhrr::new(1024, 11).unwrap();
        let role = fhrr.atom("role:owner");
        let filler = fhrr.atom("filler:user");
        let bound = fhrr.bind(&role, &filler).unwrap();
        assert_eq!(bound, fhrr.bind(&filler, &role).unwrap());
        let recovered = fhrr.unbind(&bound, &role).unwrap();
        assert!(fhrr.cosine(&recovered, &filler).unwrap() > 0.999_999);
    }

    #[test]
    fn bundled_role_filler_pairs_cleanup_against_distractors() {
        let fhrr = Fhrr::new(2048, 13).unwrap();
        let roles: Vec<_> = (0..8).map(|i| fhrr.atom(&format!("role:{i}"))).collect();
        let fillers: Vec<_> = (0..8).map(|i| fhrr.atom(&format!("filler:{i}"))).collect();
        let distractors: Vec<_> = (0..64)
            .map(|i| fhrr.atom(&format!("distractor:{i}")))
            .collect();
        let pairs: Vec<_> = roles
            .iter()
            .zip(&fillers)
            .map(|(role, filler)| fhrr.bind(role, filler).unwrap())
            .collect();
        let hologram = fhrr.superpose(&pairs).unwrap();

        for (target, role) in roles.iter().enumerate() {
            let estimate = fhrr.unbind(&hologram, role).unwrap();
            let filler_labels: Vec<_> = (0..fillers.len()).map(|i| format!("filler:{i}")).collect();
            let distractor_labels: Vec<_> = (0..distractors.len())
                .map(|i| format!("distractor:{i}"))
                .collect();
            let dictionary = filler_labels
                .iter()
                .zip(&fillers)
                .map(|(label, vector)| (label.as_str(), vector))
                .chain(
                    distractor_labels
                        .iter()
                        .zip(&distractors)
                        .map(|(label, vector)| (label.as_str(), vector)),
                );
            let hit = fhrr.cleanup(&estimate, dictionary, 1).unwrap().remove(0);
            assert_eq!(hit.label, format!("filler:{target}"));
        }
    }

    #[test]
    fn incompatible_codebooks_are_rejected_even_at_the_same_dimension() {
        let first = Fhrr::new(64, 1).unwrap();
        let second = Fhrr::new(64, 2).unwrap();
        let error = first.bind(&first.atom("a"), &second.atom("b")).unwrap_err();
        assert!(matches!(error, Error::FingerprintMismatch { .. }));
    }

    #[test]
    fn cleanup_ties_are_label_deterministic() {
        let fhrr = Fhrr::new(32, 5).unwrap();
        let atom = fhrr.atom("same-vector");
        let hits = fhrr
            .cleanup(&atom, [("z", &atom), ("a", &atom)], 2)
            .unwrap();
        assert_eq!(hits[0].label, "a");
        assert_eq!(hits[1].label, "z");
    }

    #[test]
    fn zero_hit_cleanup_validates_only_the_query() {
        let fhrr = Fhrr::new(32, 5).unwrap();
        let query = fhrr.atom("query");
        let invalid_candidate = non_finite_vector(&fhrr);
        assert_eq!(
            fhrr.cleanup(&query, [("unused", &invalid_candidate)], 0),
            Ok(Vec::new())
        );
    }

    #[test]
    fn empty_superposition_is_an_error() {
        let fhrr = Fhrr::new(32, 5).unwrap();
        assert_eq!(fhrr.superpose(&[]), Err(Error::EmptySuperposition));
    }

    #[test]
    fn fingerprint_round_trips_and_rejects_unknown_algebra() {
        let fingerprint = Fingerprint::new(256, 99).unwrap();
        let json = serde_json::to_string(&fingerprint).unwrap();
        let decoded: Fingerprint = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, fingerprint);

        let mut unsupported = fingerprint;
        unsupported.operator = "some-future-binding-v2".into();
        assert!(matches!(
            Fhrr::from_fingerprint(unsupported),
            Err(Error::UnsupportedFingerprint(_))
        ));
    }

    #[test]
    fn mutable_bundle_can_remove_its_only_caller_proven_term() {
        let fhrr = Fhrr::new(64, 31).unwrap();
        let term = fhrr.bind(&fhrr.atom("role"), &fhrr.atom("target")).unwrap();
        let mut bundle = MutableBundle::new(&fhrr);
        bundle.add_bound_term(&term).unwrap();
        assert_eq!(bundle.term_count(), 1);
        bundle.subtract_known_term(&term).unwrap();
        assert_eq!(bundle.term_count(), 0);
        assert_eq!(bundle.estimate(), Err(Error::EmptyBundle));
        assert_eq!(bundle.algorithm_id(), MUTABLE_BUNDLE_OPERATOR_ID);
    }

    #[test]
    fn mutable_estimates_preserve_their_incremental_arithmetic_provenance() {
        let fhrr = Fhrr::new(64, 31).unwrap();
        let term = fhrr.bind(&fhrr.atom("role"), &fhrr.atom("target")).unwrap();
        let mut bundle = MutableBundle::new(&fhrr);
        bundle.add_bound_term(&term).unwrap();

        let estimate = bundle.estimate().unwrap();
        assert_eq!(estimate.algorithm_id(), MUTABLE_BUNDLE_OPERATOR_ID);
        assert_eq!(estimate.fingerprint(), fhrr.fingerprint());
        assert!(fhrr.cosine_mutable(&term, &estimate).unwrap() > 0.999_999);
        let unbound = fhrr.unbind_mutable(&estimate, &fhrr.atom("role")).unwrap();
        assert_eq!(unbound.algorithm_id(), MUTABLE_BUNDLE_OPERATOR_ID);
    }

    #[test]
    fn mutable_accumulator_matches_an_independent_coordinate_oracle() {
        let fhrr = Fhrr::new(17, 41).unwrap();
        let first = fhrr
            .bind(&fhrr.atom("role:a"), &fhrr.atom("target:a"))
            .unwrap();
        let second = fhrr
            .bind(&fhrr.atom("role:b"), &fhrr.atom("target:b"))
            .unwrap();
        let third = fhrr
            .bind(&fhrr.atom("role:c"), &fhrr.atom("target:c"))
            .unwrap();
        let mut bundle = MutableBundle::new(&fhrr);

        bundle.add_bound_term(&first).unwrap();
        bundle.add_bound_term(&second).unwrap();
        bundle.subtract_known_term(&first).unwrap();
        bundle.add_bound_term(&third).unwrap();

        assert_eq!(bundle.term_count(), 2);
        assert_eq!(
            bundle.coordinates,
            coordinate_accumulator_oracle(&[
                (1.0, &first),
                (1.0, &second),
                (-1.0, &first),
                (1.0, &third)
            ])
        );
    }

    #[test]
    fn caller_proven_replacement_is_atomic_and_preserves_count() {
        let fhrr = Fhrr::new(17, 47).unwrap();
        let incompatible_fhrr = Fhrr::new(17, 48).unwrap();
        let old = fhrr.bind(&fhrr.atom("role"), &fhrr.atom("old")).unwrap();
        let new = fhrr.bind(&fhrr.atom("role"), &fhrr.atom("new")).unwrap();
        let incompatible = incompatible_fhrr.atom("incompatible");
        let mut bundle = MutableBundle::new(&fhrr);
        bundle.add_bound_term(&old).unwrap();

        let before_failure = bundle.clone();
        assert!(matches!(
            bundle.replace_known_term(&old, &incompatible),
            Err(Error::FingerprintMismatch { .. })
        ));
        assert_eq!(bundle, before_failure);

        bundle.replace_known_term(&old, &new).unwrap();
        assert_eq!(bundle.term_count(), 1);
        assert_eq!(
            bundle.coordinates,
            coordinate_accumulator_oracle(&[(1.0, &old), (-1.0, &old), (1.0, &new)])
        );

        let mut non_finite_new = new.clone();
        non_finite_new.coordinates[0].re = f32::NAN;
        let before_non_finite = bundle.clone();
        assert_eq!(
            bundle.replace_known_term(&new, &non_finite_new),
            Err(Error::NonFiniteState)
        );
        assert_eq!(bundle, before_non_finite);

        let mut non_finite_state = bundle.clone();
        non_finite_state.coordinates[0].im = f32::INFINITY;
        let before_invalid_state = non_finite_state.clone();
        assert_eq!(
            non_finite_state.replace_known_term(&new, &old),
            Err(Error::NonFiniteState)
        );
        assert_eq!(non_finite_state, before_invalid_state);

        let mut overflow_state = bundle.clone();
        overflow_state.coordinates[0].re = f32::MAX;
        let mut subtracting = old.clone();
        subtracting.coordinates[0].re = -f32::MAX;
        let before_overflow = overflow_state.clone();
        assert_eq!(
            overflow_state.replace_known_term(&subtracting, &new),
            Err(Error::NonFiniteState)
        );
        assert_eq!(overflow_state, before_overflow);
    }

    #[test]
    fn mutable_bundle_rejects_invalid_updates_without_mutation() {
        let first = Fhrr::new(64, 32).unwrap();
        let second = Fhrr::new(64, 33).unwrap();
        let term = first
            .bind(&first.atom("role"), &first.atom("target"))
            .unwrap();
        let incompatible = second
            .bind(&second.atom("role"), &second.atom("target"))
            .unwrap();
        let mut bundle = MutableBundle::new(&first);
        let empty = bundle.clone();

        assert!(matches!(
            bundle.subtract_known_term(&term),
            Err(Error::BundleCountUnderflow)
        ));
        assert_eq!(bundle, empty);
        assert!(matches!(
            bundle.add_bound_term(&incompatible),
            Err(Error::FingerprintMismatch { .. })
        ));
        assert_eq!(bundle, empty);

        bundle.add_bound_term(&term).unwrap();
        let populated = bundle.clone();
        assert!(matches!(
            bundle.subtract_known_term(&incompatible),
            Err(Error::FingerprintMismatch { .. })
        ));
        assert_eq!(bundle, populated);
    }

    #[test]
    fn mutable_bundle_count_overflow_and_non_finite_updates_do_not_mutate() {
        let fhrr = Fhrr::new(8, 37).unwrap();
        let term = fhrr.atom("term");
        let mut overflowed = MutableBundle::new(&fhrr);
        overflowed.term_count = usize::MAX;
        let before_overflow = overflowed.clone();
        assert_eq!(
            overflowed.add_bound_term(&term),
            Err(Error::BundleCountOverflow)
        );
        assert_eq!(overflowed, before_overflow);

        let mut bundle = MutableBundle::new(&fhrr);
        let before_non_finite = bundle.clone();
        assert_eq!(
            bundle.add_bound_term(&non_finite_vector(&fhrr)),
            Err(Error::NonFiniteState)
        );
        assert_eq!(bundle, before_non_finite);
    }

    #[test]
    fn algebra_fails_closed_for_non_finite_inputs_and_outputs() {
        let fhrr = Fhrr::new(2, 43).unwrap();
        let finite = fhrr.atom("finite");
        let non_finite = non_finite_vector(&fhrr);
        assert_eq!(fhrr.bind(&finite, &non_finite), Err(Error::NonFiniteState));
        assert_eq!(fhrr.conjugate(&non_finite), Err(Error::NonFiniteState));
        assert_eq!(
            fhrr.unbind(&finite, &non_finite),
            Err(Error::NonFiniteState)
        );
        assert_eq!(
            fhrr.superpose(&[finite.clone(), non_finite]),
            Err(Error::NonFiniteState)
        );
        assert_eq!(
            fhrr.cosine(&finite, &non_finite_vector(&fhrr)),
            Err(Error::NonFiniteState)
        );
        assert_eq!(
            fhrr.cleanup(&finite, [("bad", &non_finite_vector(&fhrr))], 1),
            Err(Error::NonFiniteState)
        );

        let huge = Vector {
            coordinates: vec![
                Complex32 {
                    re: f32::MAX,
                    im: f32::MAX
                };
                2
            ],
            fingerprint: fhrr.fingerprint.clone(),
        };
        assert_eq!(fhrr.bind(&huge, &huge), Err(Error::NonFiniteState));
        assert_eq!(
            fhrr.superpose(&[huge.clone(), huge]),
            Err(Error::NonFiniteState)
        );
    }

    #[test]
    fn payload_dimension_ceiling_prevents_saturating_accounting() {
        assert_eq!(
            Fingerprint::new(MAX_DIMENSION + 1, 1),
            Err(Error::DimensionTooLarge { max: MAX_DIMENSION })
        );
        let unsupported = Fingerprint {
            version: FINGERPRINT_VERSION,
            dimension: MAX_DIMENSION + 1,
            seed: 1,
            operator: OPERATOR_ID.into(),
            atom_generator: ATOM_GENERATOR_ID.into(),
        };
        assert!(matches!(
            Fhrr::from_fingerprint(unsupported),
            Err(Error::DimensionTooLarge { max }) if max == MAX_DIMENSION
        ));
    }

    #[test]
    fn mutable_bundle_payload_reports_coordinates_and_counter_only() {
        let fhrr = Fhrr::new(11, 9).unwrap();
        let bundle = MutableBundle::new(&fhrr);
        assert_eq!(
            bundle.coordinate_payload_bytes(),
            11 * 2 * std::mem::size_of::<f32>()
        );
        assert_eq!(bundle.count_metadata_bytes(), std::mem::size_of::<usize>());
    }
}
