package security

import (
	"crypto/aes"
	"crypto/cipher"
	"crypto/rand"
	"encoding/base64"
	"errors"
	"io"
)

type SecretBox struct {
	key []byte
}

func NewSecretBox(secretKeyBase64 string) (*SecretBox, error) {
	if secretKeyBase64 == "" {
		// Dev convenience: generate an ephemeral key.
		k := make([]byte, 32)
		if _, err := rand.Read(k); err != nil {
			return nil, err
		}
		return &SecretBox{key: k}, nil
	}

	k, err := base64.StdEncoding.DecodeString(secretKeyBase64)
	if err != nil {
		return nil, errors.New("APP_SECRET_KEY must be valid base64")
	}
	if len(k) != 32 {
		return nil, errors.New("APP_SECRET_KEY must decode to 32 bytes")
	}
	return &SecretBox{key: k}, nil
}

func (b *SecretBox) EncryptToString(plaintext []byte) (string, error) {
	block, err := aes.NewCipher(b.key)
	if err != nil {
		return "", err
	}
	gcm, err := cipher.NewGCM(block)
	if err != nil {
		return "", err
	}

	nonce := make([]byte, gcm.NonceSize())
	if _, err := io.ReadFull(rand.Reader, nonce); err != nil {
		return "", err
	}

	ct := gcm.Seal(nil, nonce, plaintext, nil)
	out := append(nonce, ct...)
	return base64.RawStdEncoding.EncodeToString(out), nil
}

func (b *SecretBox) DecryptString(enc string) ([]byte, error) {
	block, err := aes.NewCipher(b.key)
	if err != nil {
		return nil, err
	}
	gcm, err := cipher.NewGCM(block)
	if err != nil {
		return nil, err
	}

	buf, err := base64.RawStdEncoding.DecodeString(enc)
	if err != nil {
		return nil, errors.New("invalid ciphertext")
	}
	if len(buf) < gcm.NonceSize() {
		return nil, errors.New("invalid ciphertext")
	}

	nonce := buf[:gcm.NonceSize()]
	ct := buf[gcm.NonceSize():]
	pt, err := gcm.Open(nil, nonce, ct, nil)
	if err != nil {
		return nil, errors.New("decrypt failed")
	}
	return pt, nil
}
