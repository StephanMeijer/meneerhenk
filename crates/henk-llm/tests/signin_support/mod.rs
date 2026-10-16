//! A stand-in for OpenAI's sign-in: a signing key, ID tokens, and the key
//! set, for tests of the sign-in flow.
//!
//! The key below exists only for these tests. It signs nothing real and
//! belongs to no account.

#![allow(dead_code, clippy::unwrap_used, clippy::indexing_slicing)]

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::{Value, json};

/// The test key as a DER-encoded RSA private key, in base64.
const TEST_KEY: &[&str] = &[
    "MIIEowIBAAKCAQEAxn6v6liQy54BLmCX7ONKUFnOZ4B4F7FzuXygmEU+wjnEzPGDZD28eo93HD4fTnMce/AtN4AtdzH+x6pwOdNJ",
    "8fZoD3DzYPCzjBKYykG3ylblrYvQt0XZt3zE5rsbsgxSp9gjmgBGGthTkTRpE3IYo3IDRBxtLhyXX9FUWH7gCkzzgjgveQaZNJ/k",
    "N1PV8fT/mZ6UQSgA12GBig5C/GzU0KpbfvOyNJOxVZviA/PYdaP8DS8wP2wZUquJI5b8kBaTS4gy4RKW6cU30tF882iX/1mt/+hl",
    "x+7RbKKwz/77r8RrGsTUfAHi2xUR7NdsGZ8nQ7eKDBzEzdVJZTI6c9tfFwIDAQABAoIBABMjGrXG1Sr0cFu2z1706PDHP8mlzq5G",
    "Gwiqldcoce0K4cj6jfbnptjQJSUxbhZDrd7bIJjlwDxqMwywB4LjtpDf91dpxeU2P7FFB7sUS/Jzn1A0lypcTynMGzW9m2V2n69z",
    "/La3yQqEicYJngnUacC0WNFDxlj4Q7XmujGQBeyoJB6brQmYjbvO4tuC2rRS4YQ7N8FWO2oj1Ewc821kcVCo7Hdbsw6+p275m/EM",
    "Y8hbDsUWpGeqHSvMdBMn0S6VDIXJ5IKJB33AbYBufnqKhTaO/6QXEW/TYEroIDulLhYpPe6t5nfF6xvM/HGRTxkebHvyx3wNbPz0",
    "Du9PVzimjo0CgYEA6yQzMu8xnXXCil5o+Qhbf0IA5q+LPC2OYtsyxYuBkmpeB1stBui2w0PFotNqlO71YUZdVSckZUIY/XvXhRLB",
    "cxFvcXDAJp58sKI1w4Erle0iZt1XvmfhI+tkcNH7UcxKWJ+x5d/xuIa2oQ1Gu+rgkM4oxd2toGwmOF5Hedsr8mMCgYEA2BpI/JFi",
    "bnOphjyZLI9ED0coLquvVaelrwsu4/PpjJ7jpo7aXQBku3CU6xJ1ve9huHLaoLgdXvG8GYmmIXATD3W9Vuzdz/s6InkSPLdrWRih",
    "DpI38Wwif4cumutXCTqiMw8Lgv4D2nGJW49K+UvqQn6+fzCGPAw73SwEuMb2pL0CgYBcruAfa3i6//e2YpAt2MV326RPSVIwXiSJ",
    "gqz4gIyKEbMOYznUR1YCkWwNShfwrF+7/+g58DvATdVb1LyWj1YA7RZoOHEEx+OOG+HlNbzgU6ADu6siOf/cEKR+c8YD7+o+FHQi",
    "j6rPeQnzYTBIUcRCT4ByvXX6jaZjAYZa2bDw0QKBgQClSvWouktf0HhKhO9kdfDI/tRtXkSub5z7VsJ2+YoaVS4rp/i+Sh7CgEpn",
    "w6brsL4+Jxuy8yEEDkKiBfdcfC0YFibM5kCDZNYCqDia2HDzpTMLwiuoLd31TaEIPhpEpOSLyWY0u7aWz2EQ93mRVc/JLMjo4GTM",
    "aU8gzRqPeyAMwQKBgFf0uPsa/XNd51loUVcMEWCWuji3h93bWNN/ZRzhjVx7CM3Gm3eelFVnfplUHV/5JJvGDEdQRxaVNnG/8+io",
    "HTomNoOfsUFjnY/qM1cYjaZFC6TpfmMvTEp2AkKMYzMhcap2vOseZNBFteXbjj3goPESTCNZr1pPIYodc9sfq0sL",
];

/// The public half, as a JWK set.
pub fn jwks() -> Value {
    json!({"keys": [{
        "kty": "RSA",
        "kid": "test-key",
        "use": "sig",
        "alg": "RS256",
        "n": "xn6v6liQy54BLmCX7ONKUFnOZ4B4F7FzuXygmEU-wjnEzPGDZD28eo93HD4fTnMce_AtN4AtdzH-x6pwOdNJ8fZoD3DzYPCzjBKYykG3ylblrYvQt0XZt3zE5rsbsgxSp9gjmgBGGthTkTRpE3IYo3IDRBxtLhyXX9FUWH7gCkzzgjgveQaZNJ_kN1PV8fT_mZ6UQSgA12GBig5C_GzU0KpbfvOyNJOxVZviA_PYdaP8DS8wP2wZUquJI5b8kBaTS4gy4RKW6cU30tF882iX_1mt_-hlx-7RbKKwz_77r8RrGsTUfAHi2xUR7NdsGZ8nQ7eKDBzEzdVJZTI6c9tfFw",
        "e": "AQAB",
    }]})
}

/// `claims` as the payload of an ID token, signed with the test key under
/// the header `header`.
pub fn sign(header: &Value, claims: &Value) -> String {
    let der = base64::engine::general_purpose::STANDARD
        .decode(TEST_KEY.concat())
        .unwrap();
    let key = RsaKeyPair::from_der(&der).unwrap();
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let mut signature = vec![0_u8; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signing_input.as_bytes(),
        &mut signature,
    )
    .unwrap();
    format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature))
}

/// The header of a good ID token.
pub fn header() -> Value {
    json!({"alg": "RS256", "kid": "test-key", "typ": "JWT"})
}
