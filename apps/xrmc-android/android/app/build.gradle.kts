plugins {
    id("com.android.application")
}

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
        versionCode = 1
        versionName = "0.1.0"
        ndk {
            // XRMC_ABIS=arm64-v8a (or x86_64) builds a single-ABI APK (see build.sh).
            val abis = (System.getenv("XRMC_ABIS") ?: "arm64-v8a,x86_64").split(",")
            abiFilters += abis
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            // Local builds: signed with the debug key. Use your own keystore for distribution.
            signingConfig = signingConfigs.getByName("debug")
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
