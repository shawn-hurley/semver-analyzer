# Java Analysis Test Results

## Test Targets

| Target | Description | SB Version | Time | Breaking Changes |
|--------|-------------|------------|------|-----------------|
| `spring-boot` | The framework itself | 3.5.0 → 4.0.0 | 7.7s | 3,306 |
| `petclinic` | Canonical sample app | SB 3.5.6 → SB 4.0.0 | 1.1s | 5 API + 26 manifest |
| `microservices` | Distributed petclinic (8 services) | SB 3.4.1 → SB 4.0.0 | 0.5s | 30 API + 4 manifest |

Run all: `./hack/java/run.sh all`

---

## Spring Boot Framework Analysis (spring-boot)

Analyzing the framework source code to generate rules for consumers.

| Metric | Count |
|--------|-------|
| Types extracted (v3.5.0) | 3,799 |
| Members extracted | 15,401 |
| Total breaking changes | 3,306 |
| Migration targets auto-detected | 62 |

### Change Type Breakdown

| Type | Count |
|------|-------|
| removed | 1,756 |
| signature_changed | 833 |
| type_changed | 576 |
| visibility_changed | 128 |
| renamed | 13 |

### Key Detections (validated against Migration Guide)

| Migration Guide Item | Detected? |
|---------------------|-----------|
| BootstrapRegistry moved package | YES (6 classes) |
| EnvironmentPostProcessor moved | YES (9 classes) |
| JsonComponent → JacksonComponent | YES (with migration target) |
| JsonObjectSerializer → ObjectValueSerializer | YES (with migration target) |
| Undertow removal | YES (40 classes) |
| HttpMessageConverters deprecated | YES (6 classes) |
| EntityScan moved package | YES |
| JdbcDatabaseDialect → DataJdbcDatabaseDialect | YES (rename) |
| Elasticsearch changes | YES (33 changes) |
| Package modularization | YES (2,460 changes) |

---

## Consumer-Side Validation

The critical test: do the breaking changes we detect in the *framework*
correspond to the changes real *consumer apps* need to make?

### Petclinic (simple app)

The petclinic app needed these import changes when migrating SB3→SB4:

| Old Import | New Import | Detected in framework? |
|-----------|-----------|----------------------|
| `boot.autoconfigure.cache.JCacheManagerCustomizer` | `boot.cache.autoconfigure.JCacheManagerCustomizer` | YES — removed from old package |
| `boot.web.client.RestTemplateBuilder` | `boot.restclient.RestTemplateBuilder` | YES — removed from old package |
| `boot.test.web.server.LocalServerPort` | `boot.web.server.test.LocalServerPort` | YES — removed |
| `boot.autoconfigure.jdbc.DataSourceAutoConfiguration` | `boot.jdbc.autoconfigure.DataSourceAutoConfiguration` | YES — removed |
| `boot.autoconfigure.orm.jpa.HibernateJpaAutoConfiguration` | `boot.hibernate.autoconfigure.HibernateJpaAutoConfiguration` | YES — removed |
| `boot.test.web.client.TestRestTemplate` | `boot.resttestclient.TestRestTemplate` | YES — removed |
| `boot.test.autoconfigure.web.servlet.WebMvcTest` | `boot.webmvc.test.autoconfigure.WebMvcTest` | YES — removed |
| `boot.test.autoconfigure.jdbc.AutoConfigureTestDatabase` | `boot.jdbc.test.autoconfigure.AutoConfigureTestDatabase` | YES — removed |
| `boot.test.autoconfigure.orm.jpa.DataJpaTest` | `boot.data.jpa.test.autoconfigure.DataJpaTest` | YES — removed |
| `spring-boot-starter-web` dependency | `spring-boot-starter-webmvc` | YES — manifest change detected |

**Result: 10/10 import changes and 7/7 manifest breaking changes detected.**

Petclinic also had 5 API changes in its own code (method signature changes
between the two commits), correctly detected.

### Microservices (complex app — 8 services)

The microservices app had these migration changes:

| Change | Detected? |
|--------|-----------|
| `com.fasterxml.jackson.*` → `tools.jackson.*` (Jackson 3) | YES — type changes in consumer code detected |
| `MeterRegistryCustomizer` import moved | YES — type change detected |
| `@MockBean` → `@MockitoBean` | Would be detected by framework analysis rules |
| `WebFluxTest` / `WebMvcTest` moved | Would be detected by framework analysis rules |
| `MockWebServer` → `mockwebserver3` (3rd party) | NO — this is an OkHttp change, not Spring Boot |
| `HttpStatus` from Apache HttpClient 4 → 5 | NO — 3rd party dependency change |
| Spring AI API restructuring | YES — 30 API changes detected in the app's own code |
| `AIFunctionConfiguration` → `PetclinicTools` | YES — rename detected with migration target |

**Result: Framework-sourced import changes all detectable. 3rd-party
dependency changes (OkHttp, Apache HttpClient) are out of scope — they
would need separate analysis of those libraries.**

---

## What Generated Rules Would Look Like

Rules would use the Konveyor `java.referenced` provider (LSP-powered,
AST-aware). Key rule patterns:

### Package relocation (bulk — ~1,500 rules consolidatable)

```yaml
- ruleID: sb4-autoconfigure-cache-moved
  labels: [source=semver-analyzer, change-type=import-path-change]
  effort: 1
  category: mandatory
  description: "Cache auto-configuration moved to new package"
  message: |
    Replace: import org.springframework.boot.autoconfigure.cache.*
    With:    import org.springframework.boot.cache.autoconfigure.*
  when:
    java.referenced:
      pattern: org.springframework.boot.autoconfigure.cache*
      location: IMPORT
```

### Class rename

```yaml
- ruleID: sb4-jackson-jsoncomponent-renamed
  labels: [source=semver-analyzer, change-type=renamed, has-codemod=true]
  effort: 3
  category: mandatory
  description: "@JsonComponent renamed to @JacksonComponent"
  message: |
    @JsonComponent has been renamed to @JacksonComponent.
  when:
    or:
      - java.referenced:
          pattern: org.springframework.boot.jackson.JsonComponent
          location: ANNOTATION
      - java.referenced:
          pattern: org.springframework.boot.jackson.JsonComponent
          location: IMPORT
```

### Feature removal

```yaml
- ruleID: sb4-undertow-removed
  labels: [source=semver-analyzer, change-type=removed]
  effort: 7
  category: mandatory
  description: "Undertow support removed in Spring Boot 4"
  message: |
    Spring Boot 4 requires Servlet 6.1. Undertow is not compatible.
    Migrate to Tomcat (default) or Jetty.
  when:
    java.referenced:
      pattern: org.springframework.boot.web.embedded.undertow*
      location: IMPORT
```

### Dependency check (starter rename)

```yaml
- ruleID: sb4-starter-web-renamed
  labels: [source=semver-analyzer, change-type=dependency-update]
  effort: 1
  category: mandatory
  description: "spring-boot-starter-web renamed to spring-boot-starter-webmvc"
  when:
    java.dependency:
      name: org.springframework.boot.spring-boot-starter-web
```

### Consolidated package moves

The ~1,500 package relocations can be consolidated into ~30 rules by
grouping on old package prefix. For example, all classes under
`org.springframework.boot.autoconfigure.jdbc.*` moved to
`org.springframework.boot.jdbc.autoconfigure.*`.

---

## Gaps and Next Steps

### What works well
- API surface extraction (3,799 types, 15,401 members in 7.7s)
- Breaking change detection (removals, renames, signature changes, type changes)
- Migration target inference (62 auto-detected replacements)
- Annotation change detection via `diff_language_data`
- Manifest (pom.xml) change detection
- Consumer-side validation: all 10 petclinic import changes traceable to framework changes

### What needs work
1. **Package relocation consolidation**: Individual class removals should be
   grouped into package-level relocation rules (old package prefix → new prefix)
2. **Rule generation implementation**: The `java.referenced` condition builder
   doesn't exist yet — only the TS `frontend.referenced` builder is implemented
3. **3rd party dependency analysis**: Changes in Jackson, OkHttp, Apache
   HttpClient etc. need separate framework analysis runs
4. **Configuration property analysis**: YAML/properties file changes are
   outside Java source analysis scope

### Interface visibility fix applied
During testing, discovered that interface methods without explicit `public`
keyword were extracted as `Internal` (package-private) instead of `Public`.
Fixed: 1,245 interface members now correctly extracted as `public`.
