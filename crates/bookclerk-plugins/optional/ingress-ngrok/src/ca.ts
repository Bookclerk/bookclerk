/**
 * The root certificate ngrok signs its agent endpoint with.
 *
 * `connect.ngrok-agent.com` presents a chain that ends in ngrok's own
 * private CA ("Root CA", OU prod, O ngrok Inc), not in a public root, so
 * the connection trusts only this anchor. Copied from
 * `golang.ngrok.com/ngrok` v1.12.1 `assets/ngrok.ca.crt` (MIT, Copyright
 * 2022 ngrok, Inc.). SHA-256 fingerprint
 * 59:6E:85:A5:3D:95:5D:3D:57:24:E9:42:7C:2F:2C:62:18:70:D9:F6:72:0B:68:F9:A0:A8:B2:6E:8B:6E:14:94.
 */
export const NGROK_ROOT_CA = `-----BEGIN CERTIFICATE-----
MIID4TCCAsmgAwIBAgIUZqF2AkB17pISojTndgc2U5BDt74wDQYJKoZIhvcNAQEL
BQAwbzEQMA4GA1UEAwwHUm9vdCBDQTENMAsGA1UECwwEcHJvZDESMBAGA1UECgwJ
bmdyb2sgSW5jMRYwFAYDVQQHDA1TYW4gRnJhbmNpc2NvMRMwEQYDVQQIDApDYWxp
Zm9ybmlhMQswCQYDVQQGEwJVUzAgFw0yMjA4MzExNTE3MjFaGA80NzYwMDcyODE1
MTcyMVowbzEQMA4GA1UEAwwHUm9vdCBDQTENMAsGA1UECwwEcHJvZDESMBAGA1UE
CgwJbmdyb2sgSW5jMRYwFAYDVQQHDA1TYW4gRnJhbmNpc2NvMRMwEQYDVQQIDApD
YWxpZm9ybmlhMQswCQYDVQQGEwJVUzCCASIwDQYJKoZIhvcNAQEBBQADggEPADCC
AQoCggEBAMPkZpOguChG8QXfp1eCu21wipptiWO9U6F2DRf5ln8XXAAokZyfo4IZ
795G+KdkEbq4KxSXHehhKQFDwlFnzIkZsDu6PHabXsutAmNLmoRQzsETTdh3gMEJ
JiCW+mtqmbWPH22GXnUXxe5R6dWbkXqrITy6nFpZWdFbKmo9/1VoyWdIgcXujq2D
aNCWm2BoQ9seCebc5+6gF2syXzvoKVZ4qg6O1anCl1K0ZH/2mDXu/22O2U4Tr/j7
6Da1Y7TWZYDU2dIz+tyfTOMrlaxXyxxmXewzOpYjBiHisfPpz7AtrTlAzaEVVhRk
c86vC2h42zqH8Jv0fjJdfMkVXe3eegECAwEAAaNzMHEwHQYDVR0OBBYEFNxeUxPI
M8G7cX0DhFc81pLD4W+HMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgEG
MC8GA1UdHwQoMCYwJKAioCCGHmh0dHA6Ly9jcmwubmdyb2suY29tL25ncm9rLmNy
bDANBgkqhkiG9w0BAQsFAAOCAQEAChXl+eYIQbn0OOHLuCBvXxDKHqccJLPaxJR1
LeWj8HjWbyLXnS405YNn84NFirpYzemeYSex+os92kjjLhBXEOIEpAE9JebDk7N5
X4xSOkS7vrOepX4JFNhqVdxut7pqEmuj1Xf7KhHtFquFM5fhLJHnWEJGWOTRbRVp
KWqZI/HzaltSbgiikf3S2qu6oZHph/BpueCqLKwvJziPQGE+cYdYQzRKPJZbuorj
+CnYUXd7kHC3RZzs6egVIvUYy+bGgv1CeeAm9EccL2RmPkSzThOo6oXBLR50Zlke
1x7y/5om6zp9vGTW4PWVAW/VWw1x4zxtSQ7NrP1Ldh7Xmnb7sw==
-----END CERTIFICATE-----
`;
