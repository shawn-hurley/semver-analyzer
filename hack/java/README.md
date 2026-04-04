# Java Analysis Test Harness

Test the Java language implementation against Spring Boot 3 → 4 migrations.

## Test Targets

| Target | Repository | What it tests |
|--------|-----------|---------------|
| `spring-boot` | spring-projects/spring-boot (v3.5.0 → v4.0.0) | Framework-level breaking changes. Generates the rules. |
| `petclinic` | spring-projects/spring-petclinic (SB3 → SB4) | Simple consumer app. Validates rules catch real migration needs. |
| `microservices` | spring-petclinic/spring-petclinic-microservices (SB3.4 → SB4) | Complex multi-service app. Spring Cloud, AI, Resilience4j, Eureka. |

## Usage

```bash
# Analyze a single target
./hack/java/run.sh spring-boot
./hack/java/run.sh petclinic
./hack/java/run.sh microservices

# Run all three
./hack/java/run.sh all

# Extract API surface only (for inspection)
./hack/java/run.sh --extract-only petclinic 66747e3
```

## Environment Variables

- `SPRING_BOOT_DIR` — Override spring-boot clone location (default: `/tmp/spring-boot`)

## Output

Results in `hack/java/output/`:
- `report-spring-boot.json` — Framework analysis (3,306 breaking changes)
- `report-petclinic.json` — Petclinic migration analysis
- `report-microservices.json` — Microservices migration analysis
- `*.log` — Debug logs

## Results

See [RESULTS.md](RESULTS.md) for detailed comparison against the
Spring Boot 4.0 Migration Guide and consumer app validation.

## Konveyor Rule Generation

Generated rules would use the `java.referenced` provider from the
Konveyor analyzer-lsp. See RESULTS.md for example rule YAML using
conditions like `java.referenced` with `IMPORT`, `ANNOTATION`, `TYPE`
locations, and `java.dependency` for starter POM renames.
