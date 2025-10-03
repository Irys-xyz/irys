# Reference

This section contains authoritative reference materials for developers working on the Irys node. These documents provide prescriptive guidance, canonical lists, and quick-lookup information that applies across the entire codebase.

## Contents

### [Versioning Strategy](./01-versioning-strategy.md)
Guidelines for versioning different categories of data structures in the node:
- **Canonical Data** - Chain-persisted data requiring strict forward-compatibility
- **Internal Runtime Data** - Ephemeral data with loose versioning via DB migrations  
- **Transient Protocol Data** - Network I/O structures requiring interop compatibility

Essential reading when designing new data structures or evolving existing ones.

### [Glossary](./02-glossary.md)
Definitions of technical terms, acronyms, and domain-specific concepts used throughout the Irys protocol and node implementation.

Quick reference for understanding terminology like:
- DA (Data Availability)
- Packing/unpacking
- Commitment types
- EMA (Exponential Moving Average)
- And more...

### [Error Codes](./03-error-codes.md)
Complete reference of error codes returned by the node, including:
- Error code ranges by component
- Error descriptions and causes
- Recommended remediation steps
- Related configuration options

### [Configuration Reference](./04-configuration-reference.md)
Comprehensive documentation of all configuration options available for the Irys node:
- Environment variables
- Config file parameters
- Command-line flags
- Default values and valid ranges
- Performance tuning recommendations

---

## Related Sections

- **[Architecture](../10-architecture/README.md)** - How these reference materials apply to the overall system design
- **[Domain Models](../85-domain-models/README.md)** - See how versioning strategy applies to domain models
- **[Services](../90-services/README.md)** - Message protocols follow the transient protocol data versioning guidelines

---

## Contributing

When adding new reference materials:
- Keep documents focused and scannable
- Use tables and lists for quick lookup
- Cross-reference related documentation
- Update this README with new additions
- Ensure examples are concrete and practical