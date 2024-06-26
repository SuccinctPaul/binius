// Copyright 2024 Ulvetanna Inc.

//! This an example SNARK for proving the P permutation of the Grøstl-256 hash function.
//!
//! The Grøstl hash function family is based on two permutations P and Q, which are nearly
//! identical aside from a few constants. Both permutations are used in the compression function
//! and the P permutation is additional used to finalize the hash digest.

#![feature(array_try_from_fn)]
#![feature(array_try_map)]
#![feature(step_trait)]

use anyhow::{ensure, Result};
use binius_core::{
	oracle::{MultilinearOracleSet, OracleId, ShiftVariant},
	polynomial::{
		composition::{empty_mix_composition, index_composition},
		transparent::{
			constant::Constant, multilinear_extension::MultilinearExtensionTransparent,
			step_down::StepDown,
		},
		CompositionPoly, Error as PolynomialError, MultilinearComposite, MultilinearExtension,
		MultilinearPoly,
	},
};
use binius_field::{
	packed::set_packed_slice, AESTowerField128b, AESTowerField8b, BinaryField128b, BinaryField1b,
	BinaryField8b, ExtensionField, Field, PackedAESBinaryField32x8b, PackedAESBinaryField64x8b,
	PackedBinaryField16x8b, PackedBinaryField256x1b, PackedField, PackedFieldIndexable, TowerField,
};
use binius_hash::Groestl256Core;
use binius_macros::composition_poly;
use itertools::chain;
use rand::thread_rng;
use std::{array, env, iter, slice, sync::Arc};
use tracing::instrument;
use tracing_profile::{CsvLayer, PrintTreeConfig, PrintTreeLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Number of rounds in a Grøstl-256 compression
const N_ROUNDS: usize = 10;
/// Smallest value such that 2^LOG_COMPRESSION_BLOCK >= N_ROUNDS
const LOG_COMPRESSION_BLOCK: usize = 4;

/// Constant vector of the Rijndael S-box affine transformation.
const SBOX_VEC: AESTowerField8b = AESTowerField8b::new(0x63);
/// Matrix columns of the Rijndael S-box affine transformation.
const SBOX_MATRIX: [AESTowerField8b; 8] = [
	AESTowerField8b::new(0b00011111),
	AESTowerField8b::new(0b00111110),
	AESTowerField8b::new(0b01111100),
	AESTowerField8b::new(0b11111000),
	AESTowerField8b::new(0b11110001),
	AESTowerField8b::new(0b11100011),
	AESTowerField8b::new(0b11000111),
	AESTowerField8b::new(0b10001111),
];
/// The first row of the circulant matrix defining the MixBytes step in Grøstl.
const MIX_BYTES_VEC: [AESTowerField8b; 8] = [
	AESTowerField8b::new(0x02),
	AESTowerField8b::new(0x02),
	AESTowerField8b::new(0x03),
	AESTowerField8b::new(0x04),
	AESTowerField8b::new(0x05),
	AESTowerField8b::new(0x03),
	AESTowerField8b::new(0x05),
	AESTowerField8b::new(0x07),
];

fn init_tracing() {
	if let Ok(csv_path) = env::var("PROFILE_CSV_FILE") {
		let _ = tracing_subscriber::registry()
			.with(CsvLayer::new(csv_path))
			.with(tracing_subscriber::fmt::layer())
			.try_init();
	} else {
		let _ = tracing_subscriber::registry()
			.with(PrintTreeLayer::new(PrintTreeConfig {
				attention_above_percent: 25.0,
				relevant_above_percent: 2.5,
				hide_below_percent: 1.0,
				display_unaccounted: false,
			}))
			.with(tracing_subscriber::fmt::layer())
			.try_init();
	}
}

fn p_round_consts() -> [Vec<PackedBinaryField16x8b>; 8] {
	let mut p_round_consts = [PackedBinaryField16x8b::zero(); 8];
	for i in 0..8 {
		let p_round_consts =
			PackedFieldIndexable::unpack_scalars_mut(slice::from_mut(&mut p_round_consts[i]));
		for r in 0..N_ROUNDS {
			p_round_consts[r] = AESTowerField8b::new(((i * 0x10) ^ r) as u8).into();
		}
	}
	p_round_consts.map(|p_round_consts_i| vec![p_round_consts_i])
}

#[derive(Debug)]
struct TraceOracle {
	// Transparent columns
	/// Single-bit selector of whether a round should link its output to the next input.
	round_selector: OracleId,
	/// Default round constant for P permutation
	p_default_round_const: OracleId,
	/// Round constants for P permutation, aside from the default
	p_round_consts: [OracleId; 8],

	// Public columns
	/// Round input state
	p_in: [OracleId; 64],
	/// Round output state
	p_out: [OracleId; 64],
	// Committed witness columns
	/// Bits of the S-box inverse in the SubBytes step, decomposed using the AES field basis.
	p_sub_bytes_inv_bits: [OracleId; 64 * 8],
	/// The product of the input and its inverse. The value is either one or zero in a valid
	/// witness.
	p_sub_bytes_prod: [OracleId; 64],
	// Virtual witness columns
	/// The S-box inverse in the SubBytes step, defined as a linear combination of
	/// `p_sub_bytes_inv_bits`.
	p_sub_bytes_inv: [OracleId; 64],
	/// The S-box output, defined as a linear combination of `p_sub_bytes_inv_bits`.
	p_sub_bytes_out: [OracleId; 64],
	/// The next round input, defined as a shift of `p_in`.
	p_next_in: [OracleId; 64],
}

impl TraceOracle {
	fn new<F>(oracles: &mut MultilinearOracleSet<F>, log_size: usize) -> Result<Self>
	where
		F: TowerField + ExtensionField<BinaryField8b>,
	{
		// Fixed transparent columns
		let round_selector_single =
			oracles.add_transparent(StepDown::new(LOG_COMPRESSION_BLOCK, N_ROUNDS - 1)?)?;
		let round_selector =
			oracles.add_repeating(round_selector_single, log_size - LOG_COMPRESSION_BLOCK)?;

		let p_default_round_const = oracles.add_transparent(Constant {
			n_vars: log_size,
			value: F::ZERO,
		})?;
		let p_round_consts = p_round_consts().try_map(|p_round_consts_i| {
			let p_rc_single = oracles.add_transparent(MultilinearExtensionTransparent(
				MultilinearExtension::from_values(p_round_consts_i)
					.unwrap()
					.specialize::<F>(),
			))?;
			oracles.add_repeating(p_rc_single, log_size - LOG_COMPRESSION_BLOCK)
		})?;

		// Committed public & witness columns
		let mut batch_scope_1b =
			oracles.build_committed_batch(log_size, BinaryField1b::TOWER_LEVEL);
		let p_sub_bytes_inv_bits = batch_scope_1b.add_multiple::<{ 64 * 8 }>();
		let _trace1b_batch_id = batch_scope_1b.build();

		let mut batch_scope_8b =
			oracles.build_committed_batch(log_size, BinaryField8b::TOWER_LEVEL);
		let p_in = batch_scope_8b.add_multiple::<64>();
		let p_out = batch_scope_8b.add_multiple::<64>();
		let p_sub_bytes_prod = batch_scope_8b.add_multiple::<64>();
		let _trace8b_batch_id = batch_scope_8b.build();

		// Virtual witness columns
		let p_sub_bytes_inv = array::try_from_fn(|ij| {
			oracles.add_linear_combination(
				log_size,
				(0..8).map(|b| {
					let basis = BinaryField8b::from(
						<AESTowerField8b as ExtensionField<BinaryField1b>>::basis(b)
							.expect("index is less than extension degree"),
					);
					(p_sub_bytes_inv_bits[ij * 8 + b], basis.into())
				}),
			)
		})?;
		let p_sub_bytes_out = array::try_from_fn(|ij| {
			oracles.add_linear_combination_with_offset(
				log_size,
				BinaryField8b::from(SBOX_VEC).into(),
				(0..8).map(|b| {
					(p_sub_bytes_inv_bits[ij * 8 + b], BinaryField8b::from(SBOX_MATRIX[b]).into())
				}),
			)
		})?;

		let p_next_in =
			p_in.try_map(|p_in_i| oracles.add_shifted(p_in_i, 1, 4, ShiftVariant::LogicalRight))?;

		Ok(TraceOracle {
			round_selector,
			p_default_round_const,
			p_round_consts,
			p_in,
			p_out,
			p_sub_bytes_inv_bits,
			p_sub_bytes_prod,
			p_sub_bytes_inv,
			p_sub_bytes_out,
			p_next_in,
		})
	}

	fn iter_oracles(&self) -> impl Iterator<Item = OracleId> + '_ {
		chain!(
			iter::once(self.round_selector),
			iter::once(self.p_default_round_const),
			self.p_round_consts,
			self.p_sub_bytes_inv_bits,
			self.p_in,
			self.p_out,
			self.p_sub_bytes_prod,
			self.p_sub_bytes_inv,
			self.p_sub_bytes_out,
			self.p_next_in,
		)
	}

	fn p_round_const(&self, ij: usize) -> OracleId {
		let i = ij / 8;
		let j = ij % 8;
		if j == 0 {
			self.p_round_consts[i]
		} else {
			self.p_default_round_const
		}
	}
}

composition_poly!(SubBytesProductCheck[x, inv, prod, rc] = (x + rc) * inv - prod);
composition_poly!(ProductImpliesInputZero[x, prod, rc] = (x + rc) * (prod - 1));
composition_poly!(ProductImpliesInverseZero[inv, prod] = inv * (prod - 1));
composition_poly!(ConditionalEquality[x, y, is_equal] = (x - y) * is_equal);

#[derive(Debug, Clone)]
struct MixColumn<F8b: Clone> {
	mix_bytes: [F8b; 8],
}

impl<F8b: Clone + From<AESTowerField8b>> Default for MixColumn<F8b> {
	fn default() -> Self {
		Self {
			mix_bytes: MIX_BYTES_VEC.map(F8b::from),
		}
	}
}

impl<F8b, F> CompositionPoly<F> for MixColumn<F8b>
where
	F8b: Field,
	F: ExtensionField<F8b>,
{
	fn n_vars(&self) -> usize {
		9
	}

	fn degree(&self) -> usize {
		1
	}

	fn evaluate<P: PackedField<Scalar = F>>(&self, query: &[P]) -> Result<P, PolynomialError> {
		if query.len() != 9 {
			return Err(PolynomialError::IncorrectQuerySize { expected: 9 });
		}

		// This is unfortunate that it needs to unpack and repack...
		let result = iter::zip(query[1..].iter(), self.mix_bytes)
			.map(|(x_i, coeff)| P::from_fn(|j| x_i.get(j) * coeff))
			.sum::<P>();
		Ok(result - query[0])
	}

	fn binary_tower_level(&self) -> usize {
		AESTowerField8b::TOWER_LEVEL
	}
}

fn make_constraints<F8b, FW>(
	trace_oracle: &TraceOracle,
	challenge: FW,
) -> Result<impl CompositionPoly<FW>>
where
	F8b: TowerField + From<AESTowerField8b>,
	FW: TowerField + ExtensionField<F8b>,
{
	let zerocheck_column_ids = trace_oracle.iter_oracles().collect::<Vec<_>>();

	let mix = empty_mix_composition(zerocheck_column_ids.len(), challenge);

	// SubBytes product consistency
	let mix = mix.include(array::try_from_fn::<_, 64, _>(|ij| {
		index_composition(
			&zerocheck_column_ids,
			[
				trace_oracle.p_in[ij],
				trace_oracle.p_sub_bytes_inv[ij],
				trace_oracle.p_sub_bytes_prod[ij],
				trace_oracle.p_round_const(ij),
			],
			SubBytesProductCheck,
		)
	})?)?;

	// SubBytes: x * inv == 1 OR x == 0
	let mix = mix.include(array::try_from_fn::<_, 64, _>(|ij| {
		index_composition(
			&zerocheck_column_ids,
			[
				trace_oracle.p_in[ij],
				trace_oracle.p_sub_bytes_prod[ij],
				trace_oracle.p_round_const(ij),
			],
			ProductImpliesInputZero,
		)
	})?)?;

	// SubBytes: x * inv == 1 OR inv == 0
	let mix = mix.include(array::try_from_fn::<_, 64, _>(|ij| {
		index_composition(
			&zerocheck_column_ids,
			[
				trace_oracle.p_sub_bytes_inv[ij],
				trace_oracle.p_sub_bytes_prod[ij],
			],
			ProductImpliesInverseZero,
		)
	})?)?;

	// ShiftBytes + MixBytes
	let mix = mix.include(array::try_from_fn::<_, 64, _>(|ij| {
		let i = ij / 8;
		let j = ij % 8;

		let mut oracle_ids = [OracleId::default(); 9];
		oracle_ids[0] = trace_oracle.p_out[ij];
		for k in 0..8 {
			let j_prime = (j + k) % 8;
			let i_prime = (i + j_prime) % 8;
			oracle_ids[k + 1] = trace_oracle.p_sub_bytes_out[i_prime * 8 + j_prime];
		}

		index_composition(&zerocheck_column_ids, oracle_ids, MixColumn::<F8b>::default())
	})?)?;

	// consistency checks with next round
	let mix = mix.include(array::try_from_fn::<_, 64, _>(|ij| {
		index_composition(
			&zerocheck_column_ids,
			[
				trace_oracle.p_out[ij],
				trace_oracle.p_next_in[ij],
				trace_oracle.round_selector,
			],
			ConditionalEquality,
		)
	})?)?;

	Ok(mix)
}

struct TraceWitness<P1b: PackedField, P8b: PackedField> {
	/// Single-bit selector of whether a round should link its output to the next input.
	round_selector: Vec<P1b>,
	/// Default round constant for P permutation
	p_default_round_const: Vec<P8b>,
	/// Round constants for P permutation, aside from the default
	p_round_consts: [Vec<P8b>; 8],
	p_in: [Vec<P8b>; 64],
	p_out: [Vec<P8b>; 64],
	p_sub_bytes_inv_bits: [Vec<P1b>; 64 * 8],
	p_sub_bytes_prod: [Vec<P8b>; 64],
	p_sub_bytes_inv: [Vec<P8b>; 64],
	p_sub_bytes_out: [Vec<P8b>; 64],
	p_next_in: [Vec<P8b>; 64],
}

impl<P1b: PackedField, P8b: PackedField> TraceWitness<P1b, P8b> {
	fn all_polys<F>(&self) -> Result<Vec<Arc<dyn MultilinearPoly<F> + Send + Sync + '_>>>
	where
		F: ExtensionField<P1b::Scalar> + ExtensionField<P8b::Scalar>,
	{
		let fixed_polys_1b = iter::once(&self.round_selector).map(|values| {
			let mle = MultilinearExtension::from_values_slice(values.as_slice())?;
			Ok(mle.specialize_arc_dyn())
		});
		let fixed_polys_8b =
			chain!(iter::once(&self.p_default_round_const), self.p_round_consts.iter()).map(
				|values| {
					let mle = MultilinearExtension::from_values_slice(values.as_slice())?;
					Ok(mle.specialize_arc_dyn())
				},
			);
		let trace_polys_1b = self.p_sub_bytes_inv_bits.iter().map(|values| {
			let mle = MultilinearExtension::from_values_slice(values.as_slice())?;
			Ok(mle.specialize_arc_dyn())
		});
		let trace_polys_8b = chain!(
			self.p_in.iter(),
			self.p_out.iter(),
			self.p_sub_bytes_prod.iter(),
			self.p_sub_bytes_inv.iter(),
			self.p_sub_bytes_out.iter(),
			self.p_next_in.iter(),
		)
		.map(|values| {
			let mle = MultilinearExtension::from_values_slice(values.as_slice())?;
			Ok(mle.specialize_arc_dyn())
		});
		chain!(fixed_polys_1b, fixed_polys_8b, trace_polys_1b, trace_polys_8b).collect()
	}
}

fn s_box(x: AESTowerField8b) -> AESTowerField8b {
	#[rustfmt::skip]
	const S_BOX: [u8; 256] = [
		0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5,
		0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
		0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0,
		0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
		0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc,
		0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
		0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a,
		0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
		0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0,
		0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
		0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b,
		0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
		0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85,
		0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
		0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5,
		0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
		0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17,
		0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
		0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88,
		0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
		0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c,
		0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
		0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9,
		0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
		0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6,
		0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
		0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e,
		0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
		0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94,
		0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
		0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68,
		0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
	];
	let idx = u8::from(x) as usize;
	AESTowerField8b::from(S_BOX[idx])
}

#[instrument]
fn generate_trace<P1b, P8b>(log_size: usize) -> TraceWitness<P1b, P8b>
where
	P1b: PackedField<Scalar = BinaryField1b>,
	P8b: PackedFieldIndexable<Scalar = AESTowerField8b>,
{
	let build_trace_column_1b = || vec![P1b::default(); 1 << (log_size - P1b::LOG_WIDTH)];
	let build_trace_column_8b = || vec![P8b::default(); 1 << (log_size - P8b::LOG_WIDTH)];
	let mut witness = TraceWitness {
		round_selector: build_trace_column_1b(),
		p_default_round_const: build_trace_column_8b(),
		p_round_consts: array::from_fn(|_xy| build_trace_column_8b()),
		p_in: array::from_fn(|_xy| build_trace_column_8b()),
		p_out: array::from_fn(|_xy| build_trace_column_8b()),
		p_sub_bytes_inv_bits: array::from_fn(|_xy| build_trace_column_1b()),
		p_sub_bytes_prod: array::from_fn(|_xy| build_trace_column_8b()),
		p_sub_bytes_inv: array::from_fn(|_xy| build_trace_column_8b()),
		p_sub_bytes_out: array::from_fn(|_xy| build_trace_column_8b()),
		p_next_in: array::from_fn(|_xy| build_trace_column_8b()),
	};

	fn cast_8b_cols<P8b: PackedFieldIndexable<Scalar = AESTowerField8b>, const N: usize>(
		cols: &mut [Vec<P8b>; N],
	) -> [&mut [AESTowerField8b]; N] {
		cols.each_mut()
			.map(|col| PackedFieldIndexable::unpack_scalars_mut(col.as_mut_slice()))
	}

	let p_round_consts = cast_8b_cols(&mut witness.p_round_consts);
	let p_in = cast_8b_cols(&mut witness.p_in);
	let p_out = cast_8b_cols(&mut witness.p_out);
	let p_sub_bytes_inv = cast_8b_cols(&mut witness.p_sub_bytes_inv);
	let p_sub_bytes_prod = cast_8b_cols(&mut witness.p_sub_bytes_prod);
	let p_sub_bytes_out = cast_8b_cols(&mut witness.p_sub_bytes_out);
	let p_next_in = cast_8b_cols(&mut witness.p_next_in);

	let mut rng = thread_rng();
	let groestl_core = Groestl256Core::default();

	// Each round state is 1 rows
	// Each compression is 10 round states
	for compression_i in 0..1 << (log_size - LOG_COMPRESSION_BLOCK) {
		let z = compression_i << LOG_COMPRESSION_BLOCK;

		// Randomly generate the initial compression input
		let input = PackedAESBinaryField64x8b::random(&mut rng);
		let output = groestl_core.permutation_p(input);

		// Assign the compression input
		for ij in 0..64 {
			let input_elems = PackedFieldIndexable::unpack_scalars(slice::from_ref(&input));
			p_in[ij][z] = input_elems[ij];
		}

		for r in 0..1 << LOG_COMPRESSION_BLOCK {
			let z = z | r;

			// AddRoundConstant & SubBytes
			for i in 0..8 {
				for j in 0..8 {
					let ij = i * 8 + j;

					let p_sbox_in = if j == 0 {
						p_round_consts[i][z] = AESTowerField8b::new(((i * 0x10) ^ r) as u8);
						p_in[ij][z] + p_round_consts[i][z]
					} else {
						p_in[ij][z]
					};

					p_sub_bytes_inv[ij][z] = p_sbox_in.invert_or_zero();
					p_sub_bytes_prod[ij][z] = if p_sbox_in == AESTowerField8b::ZERO {
						AESTowerField8b::ZERO
					} else {
						AESTowerField8b::ONE
					};

					let inv_bits = <AESTowerField8b as ExtensionField<BinaryField1b>>::iter_bases(
						&p_sub_bytes_inv[ij][z],
					);
					for (b, bit) in inv_bits.enumerate() {
						set_packed_slice(&mut witness.p_sub_bytes_inv_bits[ij * 8 + b], z, bit);
					}

					p_sub_bytes_out[ij][z] = s_box(p_sbox_in);
				}
			}

			// ShiftBytes & MixBytes
			for i in 0..8 {
				for j in 0..8 {
					let ij = i * 8 + j;
					p_out[ij][z] = (0..8)
						.map(|k| {
							// k is the row index into the input matrix
							// i is the column index into the input matrix _after_ MixBytes
							// i_prime is the column index into the input matrix _before_ MixBytes
							let i_prime = (i + k) % 8;
							p_sub_bytes_out[i_prime * 8 + k][z] * MIX_BYTES_VEC[(8 - j + k) % 8]
						})
						.sum();
				}
			}

			// Copy round output to next round input
			if r < N_ROUNDS - 1 {
				for ij in 0..64 {
					p_in[ij][z + 1] = p_out[ij][z];
					set_packed_slice(&mut witness.round_selector, z, BinaryField1b::ONE);
				}
			}

			if r < (1 << LOG_COMPRESSION_BLOCK) - 1 {
				for ij in 0..64 {
					p_next_in[ij][z] = p_in[ij][z + 1];
				}
			}
		}

		// Assert correct output
		for ij in 0..64 {
			let output_elems = PackedFieldIndexable::unpack_scalars(slice::from_ref(&output));
			assert_eq!(p_out[ij][z + N_ROUNDS - 1], output_elems[ij]);
		}
	}

	witness
}

fn check_witness<FW, P1b: PackedField, P8b: PackedField>(
	log_size: usize,
	constraint: impl CompositionPoly<FW>,
	witness: &TraceWitness<P1b, P8b>,
) -> Result<()>
where
	FW: ExtensionField<P1b::Scalar> + ExtensionField<P8b::Scalar>,
{
	let composite = MultilinearComposite::new(log_size, constraint, witness.all_polys::<FW>()?)?;
	for z in 0..1 << log_size {
		let constraint_eval = composite.evaluate_on_hypercube(z)?;
		ensure!(constraint_eval == FW::ZERO);
	}
	Ok(())
}

fn main() {
	init_tracing();

	let log_size = 8;

	let mut oracles = MultilinearOracleSet::<BinaryField128b>::new();
	let trace_oracle = TraceOracle::new(&mut oracles, log_size).unwrap();

	let witness = generate_trace::<PackedBinaryField256x1b, PackedAESBinaryField32x8b>(log_size);

	let mut rng = thread_rng();
	let mix_challenge = <AESTowerField128b as Field>::random(&mut rng);
	let prover_composition =
		make_constraints::<AESTowerField8b, _>(&trace_oracle, mix_challenge).unwrap();

	check_witness(log_size, prover_composition, &witness)
		.expect("trace does not satisify the constraints");
}
