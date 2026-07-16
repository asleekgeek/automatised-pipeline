// language_provider::kotlin_prefixes — the Kotlin external-import fallback
// allowlist, split out of mod.rs to keep that file within the 500-line
// limit (coding-standards.md §4.1). Concern boundary: this file owns ONLY
// the JVM/Android ecosystem package-namespace data; mod.rs owns the
// trait/registry/resolution logic that consumes it.
//
// source: well-known JVM/Android package namespaces (Maven Central top
// groupIds) beyond the Kotlin/Java stdlib roots — android/androidx (AOSP),
// com.android and com.google (Android Jetpack / Google Play services /
// Gson), com.squareup + retrofit2/okhttp3/okio (Square's networking
// stack), io.reactivex + org.reactivestreams (RxJava2 / reactive-streams
// spec), org.jetbrains + org.intellij (Kotlin/IntelliJ tooling
// annotations), junit/org.junit (JUnit 4/5), org.mockito + io.mockk
// (mocking frameworks), org.koin (Kotlin DI), dagger (Google DI),
// org.slf4j (logging facade), org.apache (Apache Commons/HttpClient),
// com.fasterxml (Jackson), timber.log (Square's Timber logging wrapper).
// Fallback list (issue #31); Gradle-coordinate-driven classification is a
// tracked follow-up, not implemented here.
pub const KOTLIN_JVM_ANDROID_EXTERNAL_PREFIXES: &[&str] = &[
    "kotlin", "kotlinx", "java", "javax", "jakarta",
    "android", "androidx", "com.android", "com.google", "com.squareup",
    "retrofit2", "okhttp3", "okio", "io.reactivex", "org.reactivestreams",
    "org.jetbrains", "org.intellij", "junit", "org.junit", "org.mockito",
    "io.mockk", "org.koin", "dagger", "org.slf4j", "org.apache",
    "com.fasterxml", "timber.log",
];
