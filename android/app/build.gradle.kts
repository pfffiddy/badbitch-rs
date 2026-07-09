plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.llmdesk.app"
    compileSdk = 34

    defaultConfig {
        applicationId = "dev.llmdesk.app"
        // Android 10 (API 29) — LG K51 / LM-K500UM, the target device.
        minSdk = 29
        targetSdk = 34
        versionCode = 1
        versionName = "0.2.0"
        ndk {
            // The K51's Helio P35 is a 64-bit SoC but some firmware runs a
            // 32-bit userspace — ship both and let the installer pick.
            abiFilters += listOf("arm64-v8a", "armeabi-v7a")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
    // cargo-ndk drops the .so files into src/main/jniLibs/<abi>/ — the
    // default source set, so no extra configuration is needed here.
}

dependencies {
    // MUST stay in lockstep with the Rust `android-activity 0.6` dependency,
    // which vendors the GameActivity 2.0.2 native glue.
    implementation("androidx.games:games-activity:2.0.2")
    implementation("androidx.core:core-ktx:1.13.1")
    // GameActivity extends AppCompatActivity, so appcompat must be on the
    // classpath (and an AppCompat theme applied — see AndroidManifest).
    implementation("androidx.appcompat:appcompat:1.7.0")
}
