use ark_ec::{AffineRepr, CurveGroup};
use ark_serialize::CanonicalSerialize;
use std::{collections::BTreeMap, sync::Arc, time::SystemTime};

use crate::{
    crypto::{
        primitives::{CurveGroups, DomainSeparationTags, ProofTranscript, Scalar, G1},
        proofs::UnifiedProof,
        SerializableG1,
    },
    errors::{Error, Result},
};

pub struct AccumulatorSystem {
    groups: Arc<CurveGroups>,
    transcript: ProofTranscript,
    current_accumulator: G1,
    historical_accumulators: Vec<HistoricalAccumulator>,
    epoch_boundaries: BTreeMap<u64, EpochBoundary>,
}

#[derive(Clone, Debug)]
pub struct HistoricalAccumulator {
    pub value: SerializableG1,
    pub epoch: u64,
    pub transcript_binding: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct EpochBoundary {
    pub start_accumulator: G1,
    pub end_accumulator: G1,
    pub(crate) transition_witness: Scalar,
}

#[derive(Clone, Debug)]
pub struct AccumulationProof {
    pub old_accumulator: G1,
    pub new_accumulator: G1,
    pub transition_metadata: Vec<u8>,
    pub witness: Scalar,
}

impl AccumulatorSystem {
    pub fn new(groups: Arc<CurveGroups>) -> Self {
        Self {
            groups: Arc::clone(&groups),
            transcript: ProofTranscript::new(
                DomainSeparationTags::ACCUMULATOR,
                Arc::clone(&groups),
            ),
            current_accumulator: G1::zero(),
            historical_accumulators: Vec::new(),
            epoch_boundaries: BTreeMap::new(),
        }
    }

    pub fn accumulate_state(&mut self, commitment: &UnifiedProof) -> Result<HistoricalAccumulator> {
        let mut transcript = self.transcript.clone();
        let point = commitment.get_commitment();

        transcript.append_point_g1(DomainSeparationTags::COMMITMENT, &point);
        let challenge = transcript.challenge_scalar(DomainSeparationTags::ACCUMULATOR);

        self.current_accumulator =
            (self.current_accumulator + point.into_group() * challenge).into();

        let binding_scalar = transcript.challenge_scalar(b"binding");
        let mut binding = Vec::new();
        binding_scalar
            .serialize_compressed(&mut binding)
            .expect("Serialization cannot fail");

        Ok(HistoricalAccumulator {
            value: self.current_accumulator.into(),
            epoch: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            transcript_binding: binding,
        })
    }

    pub fn create_epoch_boundary(
        &mut self,
        epoch: u64,
        transition_data: &[u8],
    ) -> Result<EpochBoundary> {
        let mut transcript = self.transcript.clone();

        transcript.append_message(DomainSeparationTags::STATE_TRANSITION, transition_data);
        transcript.append_point_g1(DomainSeparationTags::ACCUMULATOR, &self.current_accumulator);

        let witness = transcript.challenge_scalar(DomainSeparationTags::STATE_TRANSITION);

        let boundary = EpochBoundary {
            start_accumulator: self.current_accumulator,
            end_accumulator: G1::zero(),
            transition_witness: witness,
        };

        let binding_scalar = transcript.challenge_scalar(b"binding");
        let mut binding = Vec::new();
        binding_scalar
            .serialize_compressed(&mut binding)
            .expect("Serialization cannot fail");

        self.epoch_boundaries.insert(epoch, boundary.clone());

        self.historical_accumulators.push(HistoricalAccumulator {
            value: self.current_accumulator.into(),
            epoch,
            transcript_binding: binding,
        });

        self.current_accumulator = G1::zero();

        Ok(boundary)
    }

    // Verify proofs across epoch boundaries
    pub fn verify_cross_epoch_proof(
        &self,
        start_epoch: u64,
        end_epoch: u64,
        proof: &AccumulationProof,
    ) -> Result<bool> {
        let start_boundary = self.epoch_boundaries.get(&start_epoch).ok_or_else(|| {
            Error::validation_failed(
                "Missing start epoch boundary",
                "Start epoch boundary not found",
            )
        })?;

        let end_boundary = self.epoch_boundaries.get(&end_epoch).ok_or_else(|| {
            Error::validation_failed("Missing end epoch boundary", "End epoch boundary not found")
        })?;

        if proof.old_accumulator != start_boundary.start_accumulator {
            return Ok(false);
        }

        let expected_end = (start_boundary.start_accumulator.into_group()
            * start_boundary.transition_witness
            + end_boundary.end_accumulator.into_group() * end_boundary.transition_witness)
            .into_affine();

        if proof.new_accumulator != expected_end {
            return Ok(false);
        }

        Ok(true)
    }

    pub fn accumulate_batch(
        &mut self,
        proofs: &[UnifiedProof],
        metadata: &[Vec<u8>],
    ) -> Result<HistoricalAccumulator> {
        let mut transcript = self.transcript.clone();
        let mut batch_accumulator = G1::zero();

        for (proof, data) in proofs.iter().zip(metadata) {
            let commitment = proof.get_commitment();
            transcript.append_message(DomainSeparationTags::PUBLIC_INPUT, data);
            transcript.append_point_g1(DomainSeparationTags::COMMITMENT, &commitment);

            let challenge = transcript.challenge_scalar(DomainSeparationTags::ACCUMULATOR);
            batch_accumulator = (batch_accumulator + commitment.into_group() * challenge).into();
        }

        self.current_accumulator = (self.current_accumulator + batch_accumulator).into_affine();

        let binding_scalar = transcript.challenge_scalar(b"binding");
        let mut binding = Vec::new();
        binding_scalar
            .serialize_compressed(&mut binding)
            .expect("Serialization cannot fail");

        Ok(HistoricalAccumulator {
            value: self.current_accumulator.into(),
            epoch: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            transcript_binding: binding,
        })
    }

    /// Creates accumulation proof between two states
    pub fn create_accumulation_proof(
        &self,
        old_state: &UnifiedProof,
        new_state: &UnifiedProof,
        transition_data: &[u8],
    ) -> Result<AccumulationProof> {
        let mut transcript = self.transcript.clone();

        // Get commitment points
        let old_commitment = old_state.get_commitment();
        let new_commitment = new_state.get_commitment();

        // Bind transition data
        transcript.append_message(DomainSeparationTags::STATE_TRANSITION, transition_data);
        transcript.append_point_g1(DomainSeparationTags::COMMITMENT, &old_commitment);
        transcript.append_point_g1(DomainSeparationTags::COMMITMENT, &new_commitment);

        // Generate witness challenge
        let witness = transcript.challenge_scalar(DomainSeparationTags::ACCUMULATOR);

        // Calculate accumulators
        let old_accumulator = old_commitment;
        let new_accumulator =
            (old_commitment.into_group() + new_commitment.into_group() * witness).into_affine();

        Ok(AccumulationProof {
            old_accumulator,
            new_accumulator,
            transition_metadata: transition_data.to_vec(),
            witness,
        })
    }

    pub fn verify_accumulation(
        &self,
        proof: &AccumulationProof,
        old_state: &UnifiedProof,
        new_state: &UnifiedProof,
    ) -> Result<bool> {
        let mut transcript = self.transcript.clone();

        let old_commitment = old_state.get_commitment();
        if old_commitment != proof.old_accumulator {
            return Ok(false);
        }

        if !proof.old_accumulator.is_on_curve() || !proof.new_accumulator.is_on_curve() {
            return Ok(false);
        }

        let e1 = self.groups.pair(&old_commitment, &self.groups.g2_generator);
        let e2 = self
            .groups
            .pair(&proof.old_accumulator, &self.groups.g2_generator);
        if e1 != e2 {
            return Ok(false);
        }

        transcript.append_message(
            DomainSeparationTags::STATE_TRANSITION,
            &proof.transition_metadata,
        );
        transcript.append_point_g1(DomainSeparationTags::COMMITMENT, &old_commitment);
        transcript.append_point_g1(
            DomainSeparationTags::COMMITMENT,
            &new_state.get_commitment(),
        );

        let challenge = transcript.challenge_scalar(DomainSeparationTags::ACCUMULATOR);

        let computed = (old_commitment.into_group()
            + new_state.get_commitment().into_group() * challenge)
            .into_affine();

        Ok(computed == proof.new_accumulator)
    }

    pub fn verify_accumulator_chain(&self, commitments: &[HistoricalAccumulator]) -> Result<bool> {
        if commitments.is_empty() {
            return Ok(true);
        }

        let mut transcript = self.transcript.clone();
        let mut current = *commitments[0].value.inner();
        transcript.append_point_g1(DomainSeparationTags::COMMITMENT, &current);
        transcript.challenge_scalar(b"binding");

        for window in commitments.windows(2) {
            let next = *window[1].value.inner();
            transcript.append_point_g1(DomainSeparationTags::COMMITMENT, &current);
            let challenge = transcript.challenge_scalar(DomainSeparationTags::ACCUMULATOR);

            let expected = (current + self.groups.g1_generator * challenge).into_affine();
            if expected != next {
                return Ok(false);
            }

            transcript.challenge_scalar(b"binding");
            current = next;
        }

        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{primitives::RandomGenerator, CircuitProof};

    fn setup_test_accumulator() -> AccumulatorSystem {
        let groups = Arc::new(CurveGroups::new());
        AccumulatorSystem::new(groups)
    }

    #[test]
    fn test_accumulator_state() {
        let mut accum = setup_test_accumulator();
        let rng = RandomGenerator::new();
        let value = rng.random_scalar();
        let point = (accum.groups.g1_generator * value).into_affine();

        let proof = UnifiedProof::Circuit(CircuitProof {
            commitments: vec![point.into()],
            witnesses: vec![value.into()],
            evaluation: point.into(),
            transcript_binding: vec![],
        });

        let commitment = accum.accumulate_state(&proof).unwrap();
        assert!(!commitment.value.is_zero());
        assert_eq!(commitment.value, accum.current_accumulator);
    }

    #[test]
    fn test_batch_accumulation() {
        let mut accum = setup_test_accumulator();
        let rng = RandomGenerator::new();
        let proofs: Vec<_> = (0..3)
            .map(|_| {
                let value = rng.random_scalar();
                let point = (accum.groups.g1_generator * value).into_affine();
                UnifiedProof::Circuit(CircuitProof {
                    commitments: vec![point.into()],
                    witnesses: vec![value.into()],
                    evaluation: point.into(),
                    transcript_binding: vec![],
                })
            })
            .collect();

        let metadata: Vec<_> = (0..3).map(|i| vec![i as u8]).collect();
        let commitment = accum.accumulate_batch(&proofs, &metadata).unwrap();
        assert!(!commitment.value.is_zero());
    }

    #[test]
    fn test_accumulation_proof() {
        let accum = setup_test_accumulator();
        let rng = RandomGenerator::new();

        let old_value = rng.random_scalar();
        let old_point = (accum.groups.g1_generator * old_value).into_affine();
        let old_proof = UnifiedProof::Circuit(CircuitProof {
            commitments: vec![old_point.into()],
            witnesses: vec![old_value.into()],
            evaluation: old_point.into(),
            transcript_binding: vec![],
        });

        let new_value = rng.random_scalar();
        let new_point = (accum.groups.g1_generator * new_value).into_affine();
        let new_proof = UnifiedProof::Circuit(CircuitProof {
            commitments: vec![new_point.into()],
            witnesses: vec![new_value.into()],
            evaluation: new_point.into(),
            transcript_binding: vec![],
        });

        let proof = accum
            .create_accumulation_proof(&old_proof, &new_proof, &[1, 2, 3])
            .unwrap();

        assert!(accum
            .verify_accumulation(&proof, &old_proof, &new_proof)
            .unwrap());
    }

    #[test]
    fn test_accumulator_chain() {
        let accum = setup_test_accumulator();
        let mut transcript = accum.transcript.clone();
        let rng = RandomGenerator::new();
        let mut commitments = Vec::new();

        let initial_point = (accum.groups.g1_generator * rng.random_scalar()).into_affine();
        transcript.append_point_g1(DomainSeparationTags::COMMITMENT, &initial_point);

        let mut binding = Vec::new();
        transcript
            .challenge_scalar(b"binding")
            .serialize_compressed(&mut binding)
            .expect("Serialization cannot fail");

        commitments.push(HistoricalAccumulator {
            value: initial_point.into(),
            epoch: 0,
            transcript_binding: binding,
        });

        let mut current = initial_point;
        for i in 1..3 {
            transcript.append_point_g1(DomainSeparationTags::COMMITMENT, &current);
            let challenge = transcript.challenge_scalar(DomainSeparationTags::ACCUMULATOR);
            let accumulated = (current + accum.groups.g1_generator * challenge).into_affine();

            let mut binding = Vec::new();
            transcript
                .challenge_scalar(b"binding")
                .serialize_compressed(&mut binding)
                .expect("Serialization cannot fail");

            commitments.push(HistoricalAccumulator {
                value: accumulated.into(),
                epoch: i,
                transcript_binding: binding,
            });

            current = accumulated;
        }

        assert!(accum.verify_accumulator_chain(&commitments).unwrap());
    }
}
