The `https-cert.der` and `https-key.der` files are a synthetic, self-signed
RSA certificate/key pair for offline HTTPS regression tests. The key is
public test data and must never be used by a real service. The certificate
names only `localhost` and `127.0.0.1`, is valid from 2020 through 2120,
and is trusted only by the injected test client configuration. Production
clients retain Mozilla's normal trust roots and certificate verification.

Generated with OpenSSL: `req -new -newkey rsa:2048 -noenc` followed by
`ca -selfsign -startdate 20200101000000Z -enddate 21200101000000Z` with
`basicConstraints=critical,CA:false`, server-auth extended key usage,
digital-signature/key-encipherment key usage, and the two SANs above.
`x509 -outform DER` and `pkcs8 -topk8 -nocrypt -outform DER` produced the
checked-in fixtures. No real Figma or client data is involved.
