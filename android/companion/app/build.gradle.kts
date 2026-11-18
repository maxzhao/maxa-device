import java.security.MessageDigest

plugins { id("com.android.application"); id("org.jetbrains.kotlin.android") }

val appVersion = providers.gradleProperty("companionVersion").get()
val companionSourceSha = providers.gradleProperty("companionSourceSha256").orElse("")
val generatedAssets = layout.buildDirectory.dir("generated/payload")
val payload = tasks.register("preparePayload") {
    val agentDir = rootDir.parentFile.resolve("dist/agent")
    inputs.files(agentDir.resolve("install.sh"), agentDir.resolve("maxa-device-agent-$appVersion-aarch64-linux-android.tar.gz"))
    inputs.property("version", appVersion)
    inputs.property("companionSourceSha256", companionSourceSha)
    outputs.dir(generatedAssets)
    doLast {
        val out = generatedAssets.get().asFile
        // Remove obsolete names too: AGP expands *.gz assets during merging.
        check(out.deleteRecursively()) { "Cannot clear generated payload assets" }
        check(out.mkdirs()) { "Cannot create generated payload assets" }
        val fileName = "maxa-device-agent-$appVersion-aarch64-linux-android.tar.gz"
        val script = agentDir.resolve("install.sh")
        val archive = agentDir.resolve(fileName)
        val placeholder = !script.isFile || !archive.isFile
        if (placeholder) {
            logger.warn("PLACEHOLDER PAYLOAD: Agent outputs absent; installer intentionally refuses installation")
            out.resolve("install.sh").writeText("#!/bin/sh\necho 'PLACEHOLDER: Agent payload not built; obtain the complete same-version APK.' >&2\nexit 1\n")
            out.resolve("agent.payload").writeText("PLACEHOLDER: missing Agent $appVersion\n")
        } else {
            script.copyTo(out.resolve("install.sh"), overwrite = true)
            archive.copyTo(out.resolve("agent.payload"), overwrite = true)
        }
        fun digest(name: String) = MessageDigest.getInstance("SHA-256").digest(out.resolve(name).readBytes()).joinToString("") { "%02x".format(it) }
        out.resolve("manifest.json").writeText("""{"protocol":1,"version":"$appVersion","installSha256":"${digest("install.sh")}","agentSha256":"${digest("agent.payload")}","agentFile":"$fileName"}""")
        val sourceSha = companionSourceSha.get()
        check(sourceSha.isEmpty() || sourceSha.matches(Regex("[0-9a-f]{64}"))) { "Invalid corresponding source checksum" }
        val sourceHash = if (sourceSha.isEmpty()) "null" else "\"$sourceSha\""
        out.resolve("embedded-api-source.json").writeText("""{"file":"maxa-device-companion-$appVersion-source.tar.gz","sha256":$sourceHash}""")
        out.resolve("payload-state.json").writeText("""{"placeholder":$placeholder}""")
    }
}
android {
    namespace = "com.maxa.device.companion"
    compileSdk = 35
    buildToolsVersion = "35.0.0"
    androidResources { noCompress += listOf("payload", "sh", "json") }
    defaultConfig {
        applicationId = "com.maxa.device.companion"
        minSdk = 30
        targetSdk = 35
        versionCode = providers.gradleProperty("companionVersionCode").orElse("1").get().toInt()
        versionName = appVersion
    }
    buildFeatures { buildConfig = true }
    signingConfigs { create("localRelease") {
        storeFile = file(providers.gradleProperty("signingStore").get())
        storePassword = providers.environmentVariable("MAXA_COMPANION_STORE_PASSWORD").get()
        keyAlias = "companion"
        keyPassword = providers.environmentVariable("MAXA_COMPANION_STORE_PASSWORD").get()
    } }
    buildTypes { getByName("release") { signingConfig = signingConfigs.getByName("localRelease"); isMinifyEnabled = false } }
    sourceSets.getByName("main").assets.srcDir(generatedAssets)
    sourceSets.getByName("main").assets.srcDir(layout.buildDirectory.dir("generated/notices"))
    compileOptions { sourceCompatibility = JavaVersion.VERSION_17; targetCompatibility = JavaVersion.VERSION_17 }
    kotlinOptions { jvmTarget = "17" }
    testOptions { unitTests.all { it.testLogging.showStandardStreams = true } }
    packaging { resources.excludes += setOf("META-INF/*.kotlin_module") }
}
val notices = tasks.register<Sync>("prepareNotices") {
    into(layout.buildDirectory.dir("generated/notices/licenses"))
    from(rootDir.resolve("licenses"))
    from(rootDir.resolve("../../LICENSE")) { into("project") }
    from(rootDir.resolve("../../THIRD-PARTY.md")) { into("project") }
    from(rootDir.resolve("../../licenses")) {
        include("*-LICENSE", "*-NOTICE", "*.json", "rust-supplements/*")
        into("project/original")
    }
}
tasks.named("preBuild").configure { dependsOn(payload, notices) }
dependencies { testImplementation("junit:junit:4.13.2") }
configurations.configureEach { resolutionStrategy.activateDependencyLocking() }
