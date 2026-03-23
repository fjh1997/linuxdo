import org.gradle.api.GradleException
import org.gradle.api.tasks.Sync

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

val repoRoot = rootProject.projectDir.parentFile
val generatedAssetsDir = layout.buildDirectory.dir("generated/linuxdoAssets")
val rustBinaryProvider = providers
    .environmentVariable("LINUXDO_ANDROID_RUST_BIN")
    .orElse(repoRoot.resolve("target/aarch64-linux-android/release/linuxdo-accelerator").absolutePath)

android {
    namespace = "io.linuxdo.accelerator.android"
    compileSdk = 35

    defaultConfig {
        applicationId = "io.linuxdo.accelerator.android"
        minSdk = 28
        targetSdk = 35
        versionCode = 2
        versionName = "0.1.9-android"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    sourceSets.named("main") {
        assets.srcDir(generatedAssetsDir)
    }
}

val generateLinuxdoAssets = tasks.register<Sync>("generateLinuxdoAssets") {
    into(generatedAssetsDir)

    from(repoRoot.resolve("assets/defaults")) {
        include("linuxdo-accelerator.toml")
        into("defaults")
    }

    from(rustBinaryProvider) {
        into("bin")
        rename { "linuxdo-accelerator" }
    }

    doFirst {
        val rustBinary = file(rustBinaryProvider.get())
        if (!rustBinary.exists()) {
            throw GradleException(
                "missing Rust Android binary: ${rustBinary.absolutePath}. Build it first or set LINUXDO_ANDROID_RUST_BIN"
            )
        }
    }
}

tasks.named("preBuild") {
    dependsOn(generateLinuxdoAssets)
}

dependencies {
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.appcompat:appcompat:1.7.0")
    implementation("com.google.android.material:material:1.12.0")
    implementation("com.squareup.okhttp3:okhttp:4.12.0")
    implementation("org.tomlj:tomlj:1.1.1")
}
