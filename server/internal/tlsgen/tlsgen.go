// Package tlsgen creates and loads the server's self-signed TLS certificate and
// derives the SPKI SHA-256 pin that clients use for certificate pinning.
//
// Trust is anchored on the pin (delivered out-of-band during pairing), not on a
// CA, so the certificate is given a long lifetime and reuses its key across
// regeneration is avoided by simply never regenerating once persisted.
package tlsgen

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/base64"
	"encoding/pem"
	"fmt"
	"math/big"
	"net"
	"os"
	"path/filepath"
	"time"
)

// Result holds the loaded TLS certificate and its SPKI pin.
type Result struct {
	Certificate tls.Certificate
	// SPKIPin is base64(sha256(SubjectPublicKeyInfo)) of the leaf certificate.
	// Clients pin this value; it is stable as long as the key pair is reused.
	SPKIPin string
}

// LoadOrCreate loads cert.pem/key.pem from <dataDir>/tls, generating a new
// self-signed certificate on first run. hosts are extra SAN entries (DNS or IP).
func LoadOrCreate(dataDir, commonName string, hosts []string) (*Result, error) {
	dir := filepath.Join(dataDir, "tls")
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return nil, err
	}
	certPath := filepath.Join(dir, "cert.pem")
	keyPath := filepath.Join(dir, "key.pem")

	if fileExists(certPath) && fileExists(keyPath) {
		cert, err := tls.LoadX509KeyPair(certPath, keyPath)
		if err != nil {
			return nil, fmt.Errorf("load keypair: %w", err)
		}
		pin, err := spkiPinFromCert(cert)
		if err != nil {
			return nil, err
		}
		return &Result{Certificate: cert, SPKIPin: pin}, nil
	}

	certPEM, keyPEM, err := generate(commonName, hosts)
	if err != nil {
		return nil, err
	}
	if err := os.WriteFile(certPath, certPEM, 0o600); err != nil {
		return nil, err
	}
	if err := os.WriteFile(keyPath, keyPEM, 0o600); err != nil {
		return nil, err
	}
	cert, err := tls.X509KeyPair(certPEM, keyPEM)
	if err != nil {
		return nil, err
	}
	pin, err := spkiPinFromCert(cert)
	if err != nil {
		return nil, err
	}
	return &Result{Certificate: cert, SPKIPin: pin}, nil
}

func generate(commonName string, hosts []string) (certPEM, keyPEM []byte, err error) {
	priv, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return nil, nil, err
	}
	serial, err := rand.Int(rand.Reader, new(big.Int).Lsh(big.NewInt(1), 128))
	if err != nil {
		return nil, nil, err
	}
	now := time.Now()
	tmpl := x509.Certificate{
		SerialNumber:          serial,
		Subject:               pkix.Name{CommonName: commonName, Organization: []string{"CopySync"}},
		NotBefore:             now.Add(-1 * time.Hour),
		NotAfter:              now.AddDate(10, 0, 0),
		KeyUsage:              x509.KeyUsageDigitalSignature | x509.KeyUsageKeyEncipherment,
		ExtKeyUsage:           []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
		BasicConstraintsValid: true,
	}
	tmpl.DNSNames = append(tmpl.DNSNames, "localhost")
	tmpl.IPAddresses = append(tmpl.IPAddresses, net.IPv4(127, 0, 0, 1), net.IPv6loopback)
	for _, h := range hosts {
		if ip := net.ParseIP(h); ip != nil {
			tmpl.IPAddresses = append(tmpl.IPAddresses, ip)
		} else {
			tmpl.DNSNames = append(tmpl.DNSNames, h)
		}
	}
	tmpl.IPAddresses = append(tmpl.IPAddresses, localIPs()...)

	der, err := x509.CreateCertificate(rand.Reader, &tmpl, &tmpl, &priv.PublicKey, priv)
	if err != nil {
		return nil, nil, err
	}
	certPEM = pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})
	keyDER, err := x509.MarshalPKCS8PrivateKey(priv)
	if err != nil {
		return nil, nil, err
	}
	keyPEM = pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: keyDER})
	return certPEM, keyPEM, nil
}

// spkiPinFromCert computes base64(sha256(SubjectPublicKeyInfo)) for the leaf.
func spkiPinFromCert(cert tls.Certificate) (string, error) {
	leaf := cert.Leaf
	if leaf == nil {
		if len(cert.Certificate) == 0 {
			return "", fmt.Errorf("certificate has no leaf")
		}
		parsed, err := x509.ParseCertificate(cert.Certificate[0])
		if err != nil {
			return "", err
		}
		leaf = parsed
	}
	sum := sha256.Sum256(leaf.RawSubjectPublicKeyInfo)
	return base64.StdEncoding.EncodeToString(sum[:]), nil
}

func localIPs() []net.IP {
	var ips []net.IP
	addrs, err := net.InterfaceAddrs()
	if err != nil {
		return ips
	}
	for _, a := range addrs {
		if ipnet, ok := a.(*net.IPNet); ok && !ipnet.IP.IsLoopback() {
			if ip4 := ipnet.IP.To4(); ip4 != nil {
				ips = append(ips, ip4)
			}
		}
	}
	return ips
}

func fileExists(p string) bool {
	_, err := os.Stat(p)
	return err == nil
}
