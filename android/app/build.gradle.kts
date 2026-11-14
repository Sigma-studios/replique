import javax.inject.Inject

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
    alias(libs.plugins.kotlin.serialization)
}

// restic and rclone are built from pinned source releases by
// native/build-binaries.sh; the versions live in that script only, and are
// read back here so the app can report which restic it runs.
val nativeScript: File = rootProject.file("native/build-binaries.sh")

fun pinnedVersion(name: String): String =
    Regex("^$name=(\\S+)$", RegexOption.MULTILINE).find(nativeScript.readText())?.groupValues?.get(1)
        ?: error("$name not found in $nativeScript")

/**
 * Cross-compiles restic and rclone for Android (Go, cgo through the NDK) into a
 * jniLibs folder the variants pick up as generated sources. Up to date as long
 * as the script, the trimmed rclone main and the ABIs are unchanged; the Go
 * build cache makes a rebuild after `clean` take seconds rather than minutes.
 */
abstract class BuildNativeBinaries : DefaultTask() {
    @get:InputFile
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val script: RegularFileProperty

    @get:InputFile
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val rcloneMain: RegularFileProperty

    @get:Input
    abstract val abis: ListProperty<String>

    @get:Internal
    abstract val workDir: DirectoryProperty

    @get:Internal
    abstract val sdkDir: DirectoryProperty

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @get:Inject
    abstract val execOperations: ExecOperations

    @TaskAction
    fun build() {
        val out = outputDir.get().asFile
        out.deleteRecursively()
        execOperations.exec {
            commandLine(listOf(script.get().asFile.absolutePath, out.absolutePath) + abis.get())
            environment("REPLIQUE_NATIVE_WORK", workDir.get().asFile.absolutePath)
            if (sdkDir.isPresent) environment("ANDROID_HOME", sdkDir.get().asFile.absolutePath)
        }
    }
}

val buildNativeBinaries = tasks.register<BuildNativeBinaries>("buildNativeBinaries") {
    group = "build"
    description = "Cross-compiles restic and rclone for Android from pinned sources."
    script.set(nativeScript)
    rcloneMain.set(rootProject.file("native/rclone/main.go"))
    abis.set(listOf("arm64-v8a"))
    workDir.set(rootProject.layout.buildDirectory.dir("native"))
    sdkDir.set(androidComponents.sdkComponents.sdkDirectory)
    outputDir.set(layout.buildDirectory.dir("generated/native/jniLibs"))
}

android {
    namespace = "eu.sigmadev.replique"
    // The current AndroidX releases require compiling against API 37; the app
    // still targets 36, the newest system image it has been tested on.
    compileSdk = 37
    ndkVersion = pinnedVersion("NDK_VERSION")

    defaultConfig {
        applicationId = "eu.sigmadev.replique"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        buildConfigField("String", "RESTIC_VERSION", "\"${pinnedVersion("RESTIC_VERSION")}\"")
        buildConfigField("String", "RCLONE_VERSION", "\"${pinnedVersion("RCLONE_VERSION")}\"")
        // Only arm64 binaries are built: every phone this targets is arm64, and
        // so is the emulator on an Apple Silicon Mac.
        ndk { abiFilters += "arm64-v8a" }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }

    packaging {
        jniLibs {
            // Writes android:extractNativeLibs="true" into the merged manifest:
            // restic and rclone must exist as real files in the native library
            // directory to be executed, not stay compressed inside the APK.
            useLegacyPackaging = true
            // They are Go executables, already stripped; leave them byte for
            // byte as built.
            keepDebugSymbols += setOf("**/librestic.so", "**/librclone.so")
        }
    }
}

androidComponents {
    onVariants { variant ->
        variant.sources.jniLibs?.addGeneratedSourceDirectory(buildNativeBinaries, BuildNativeBinaries::outputDir)
    }
}

// The protocol fixtures are shared with the Rust side; the unit tests read them
// in place rather than from a copy that could drift.
tasks.withType<Test>().configureEach {
    systemProperty("replique.fixtures", rootProject.file("../proto/fixtures").absolutePath)
}

dependencies {
    implementation(platform(libs.compose.bom))
    implementation(libs.compose.material3)
    implementation(libs.compose.ui)
    implementation(libs.compose.ui.tooling.preview)
    implementation(libs.compose.material.icons)
    implementation(libs.activity.compose)
    implementation(libs.core.ktx)
    implementation(libs.lifecycle.runtime.compose)
    implementation(libs.lifecycle.viewmodel.compose)
    implementation(libs.work.runtime)
    implementation(libs.serialization.json)
    implementation(libs.coroutines.android)
    implementation(libs.bouncycastle)
    implementation(libs.zxing.embedded)

    testImplementation(libs.junit)
    testImplementation(libs.coroutines.test)

    androidTestImplementation(libs.junit)
    androidTestImplementation(libs.androidx.test.runner)
    androidTestImplementation(libs.androidx.test.core)
    androidTestImplementation(libs.androidx.test.junit)
    androidTestImplementation(libs.coroutines.test)
}
