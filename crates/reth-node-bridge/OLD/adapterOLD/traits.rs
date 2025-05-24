use alloy_rpc_types::engine::{ExecutionPayloadEnvelopeV1Irys, ExecutionPayloadV1Irys};

/// The execution payload envelope type.
pub trait PayloadEnvelopeExt: Send + Sync + std::fmt::Debug {
    /// Returns the execution payload V3 from the payload
    fn execution_payload(&self) -> ExecutionPayloadV1Irys;
}

// impl PayloadEnvelopeExt for OpExecutionPayloadEnvelopeV3 {
//     fn execution_payload(&self) -> ExecutionPayloadV3 {
//         self.execution_payload.clone()
//     }
// }

// impl PayloadEnvelopeExt for ExecutionPayloadEnvelopeV3 {
//     fn execution_payload(&self) -> ExecutionPayloadV3 {
//         self.execution_payload.clone()
//     }
// }

impl PayloadEnvelopeExt for ExecutionPayloadEnvelopeV1Irys {
    fn execution_payload(&self) -> ExecutionPayloadV1Irys {
        self.execution_payload.clone()
    }
    // fn blobs_bundle(&self) -> BlobsBundleV1 {
    //     self.blobs_bundle.clone()
    // }
}
