# kotlinx.serialization keeps what it needs through its own consumer rules.
# BouncyCastle is only reached through the lightweight API used directly in
# KeyGen, so R8 can shrink the rest of it.
-dontwarn org.bouncycastle.**
