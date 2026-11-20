plugins {
    id("com.android.application")
}

// The version comes from the Rust workspace (Cargo.toml): versionCode = major*10000 + minor*100 + patch.
val appVersion: String = rootProject.file("../../../Cargo.toml").readLines()
    .first { it.startsWith("version = ") }
    .substringAfter('"').substringBefore('"')
val appVersionCode: Int = appVersion.split(".").map { it.takeWhile(Char::isDigit).toInt() }
    .let { (major, minor, patch) -> major * 10000 + minor * 100 + patch }

// Release signing: XRMC_KEYSTORE (path), XRMC_KEYSTORE_PASSWORD, XRMC_KEY_ALIAS, XRMC_KEY_PASSWORD.
// Without them, release APKs are signed with the local debug key.
val releaseKeystore: String? = System.getenv("XRMC_KEYSTORE")?.takeIf { it.isNotBlank() }

android {
    namespace = "com.dunkxr.xrmulticast"
    compileSdk = 36
    // Pin to the build-tools/NDK that exist locally so AGP never tries to download them.
    buildToolsVersion = "36.0.0"
    ndkVersion = "27.2.12479018"

    defaultConfig {
        applicationId = "com.dunkxr.xrmulticast"
        minSdk = 33 // Android 13+ (Vulkan 1.1)
        targetSdk = 36
        versionCode = appVersionCode
        versionName = appVersion
        // XRMC_ABIS=arm64-v8a (or x86_64) builds a single-ABI APK (see build.sh).
        // ABI splits (below) choose the ABIs themselves; AGP refuses both at once.
        if (System.getenv("XRMC_SPLITS") != "1") {
            ndk {
                abiFilters += (System.getenv("XRMC_ABIS") ?: "arm64-v8a,x86_64").split(",")
            }
        }
    }

    signingConfigs {
        if (releaseKeystore != null) {
            create("release") {
                storeFile = file(releaseKeystore)
                storePassword = System.getenv("XRMC_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("XRMC_KEY_ALIAS")
                keyPassword = System.getenv("XRMC_KEY_PASSWORD") ?: System.getenv("XRMC_KEYSTORE_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName(if (releaseKeystore != null) "release" else "debug")
        }
    }

    // XRMC_SPLITS=1 (build.sh with several ABIs): one APK per ABI plus a universal APK.
    if (System.getenv("XRMC_SPLITS") == "1") {
        splits {
            abi {
                isEnable = true
                reset()
                include(*(System.getenv("XRMC_ABIS") ?: "arm64-v8a,x86_64").split(",").toTypedArray())
                isUniversalApk = true
            }
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging {
        jniLibs {
            useLegacyPackaging = false
        }
    }

    androidResources {
        // scrcpy-server is a jar: keep it byte-identical (it is pushed to headsets as is).
        noCompress += listOf("scrcpy-server")
    }
}

dependencies {
    implementation("androidx.appcompat:appcompat:1.7.1")
    // Must match the GameActivity C sources vendored by android-activity 0.6.1.
    implementation("androidx.games:games-activity:4.4.0")
}
