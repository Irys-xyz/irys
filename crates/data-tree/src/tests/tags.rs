use crate::hash::TreeDomainTag;

/// Spec §3 bytes. A reorder or a renumber fails here.
#[test]
fn domain_tags_match_the_spec_bytes() {
    assert_eq!(TreeDomainTag::Leaf.as_byte(), 0x00);
    assert_eq!(TreeDomainTag::Node.as_byte(), 0x01);
    assert_eq!(TreeDomainTag::Size.as_byte(), 0x02);
    assert_eq!(TreeDomainTag::ChunkKey.as_byte(), 0x03);
    assert_eq!(TreeDomainTag::Ticket.as_byte(), 0x04);
    assert_eq!(TreeDomainTag::ChunkIndex.as_byte(), 0x08);
}
