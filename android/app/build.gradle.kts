// Packages the Rust library (built by `cargo ndk` into src/main/jniLibs) as an APK.
// See ../build-apk.sh, which does both steps.
plugins {
    id("com.android.application")
}

// Version comes from the main Cargo.toml so the APK and desktop apps always match.
val cargoVersion: String = file("../../Cargo.toml").readLines()
    .first { it.startsWith("version") }.substringAfter('"').substringBefore('"')
val parts = cargoVersion.split(".").map { it.toInt() }

android {
    namespace = "app.lagless"
    compileSdk = 34
    ndkVersion = "27.2.12479018"

    defaultConfig {
        applicationId = "app.lagless"
        minSdk = 26 // AAudio, startForegroundService
        targetSdk = 34
        versionCode = parts[0] * 10000 + parts[1] * 100 + parts[2]
        versionName = cargoVersion
        ndk { abiFilters += listOf("arm64-v8a") }
    }

    signingConfigs {
        create("release") {
            // A private key can be supplied through the environment (CI secret); otherwise the
            // repo's public key is used so that updates install over earlier versions.
            val ks = System.getenv("LAGLESS_KEYSTORE_FILE")
            storeFile = file(ks ?: "../lagless-public.keystore")
            storePassword = System.getenv("LAGLESS_KEYSTORE_PASSWORD") ?: "lagless"
            keyAlias = System.getenv("LAGLESS_KEY_ALIAS") ?: "lagless"
            keyPassword = System.getenv("LAGLESS_KEY_PASSWORD") ?: "lagless"
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("release")
        }
    }

    packaging {
        jniLibs { useLegacyPackaging = false }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}
