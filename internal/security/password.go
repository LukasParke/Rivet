package security

import (
	"crypto/rand"
	"encoding/base64"
	"errors"

	"golang.org/x/crypto/argon2"
)

type PasswordHash struct {
	// Encoded is a self-contained string with parameters + salt + hash.
	// Format: argon2id$v=19$m=<m>,t=<t>,p=<p>$<salt_b64>$<hash_b64>
	Encoded string
}

func HashPassword(password string) (PasswordHash, error) {
	if len(password) < 12 {
		return PasswordHash{}, errors.New("password must be at least 12 characters")
	}

	// Reasonable defaults for an internet-exposed app; tune later.
	var (
		memory uint32 = 64 * 1024
		timeC  uint32 = 3
		threads uint8 = 2
		keyLen uint32 = 32
		saltLen        = 16
	)

	salt := make([]byte, saltLen)
	if _, err := rand.Read(salt); err != nil {
		return PasswordHash{}, err
	}

	hash := argon2.IDKey([]byte(password), salt, timeC, memory, threads, keyLen)

	saltB64 := base64.RawStdEncoding.EncodeToString(salt)
	hashB64 := base64.RawStdEncoding.EncodeToString(hash)

	encoded := "argon2id$v=19$m=" + itoa(memory) + ",t=" + itoa(timeC) + ",p=" + itoa(uint32(threads)) + "$" + saltB64 + "$" + hashB64

	return PasswordHash{Encoded: encoded}, nil
}

func VerifyPassword(password, encoded string) (bool, error) {
	p, err := parseArgon2ID(encoded)
	if err != nil {
		return false, err
	}

	hash := argon2.IDKey([]byte(password), p.Salt, p.Time, p.Memory, p.Threads, uint32(len(p.Hash)))
	return subtleConstantTimeEq(hash, p.Hash), nil
}

type argon2Params struct {
	Memory  uint32
	Time    uint32
	Threads uint8
	Salt    []byte
	Hash    []byte
}

func parseArgon2ID(encoded string) (argon2Params, error) {
	// Expected: argon2id$v=19$m=<m>,t=<t>,p=<p>$<salt>$<hash>
	parts := split(encoded, '$')
	if len(parts) != 5 {
		return argon2Params{}, errors.New("invalid argon2id hash format")
	}
	if parts[0] != "argon2id" {
		return argon2Params{}, errors.New("unsupported hash algorithm")
	}
	if parts[1] != "v=19" {
		return argon2Params{}, errors.New("unsupported argon2 version")
	}
	mem, t, thr, err := parseArgon2Params(parts[2])
	if err != nil {
		return argon2Params{}, err
	}
	salt, err := base64.RawStdEncoding.DecodeString(parts[3])
	if err != nil {
		return argon2Params{}, errors.New("invalid salt")
	}
	hash, err := base64.RawStdEncoding.DecodeString(parts[4])
	if err != nil {
		return argon2Params{}, errors.New("invalid hash")
	}
	return argon2Params{Memory: mem, Time: t, Threads: thr, Salt: salt, Hash: hash}, nil
}

func parseArgon2Params(s string) (mem uint32, timeC uint32, threads uint8, err error) {
	// m=65536,t=3,p=2
	kv := split(s, ',')
	if len(kv) != 3 {
		return 0, 0, 0, errors.New("invalid argon2 params")
	}
	m, ok := cutPrefix(kv[0], "m=")
	if !ok {
		return 0, 0, 0, errors.New("invalid argon2 params")
	}
	tt, ok := cutPrefix(kv[1], "t=")
	if !ok {
		return 0, 0, 0, errors.New("invalid argon2 params")
	}
	p, ok := cutPrefix(kv[2], "p=")
	if !ok {
		return 0, 0, 0, errors.New("invalid argon2 params")
	}

	mem64, err := parseUint(m)
	if err != nil {
		return 0, 0, 0, errors.New("invalid argon2 memory")
	}
	t64, err := parseUint(tt)
	if err != nil {
		return 0, 0, 0, errors.New("invalid argon2 time")
	}
	p64, err := parseUint(p)
	if err != nil {
		return 0, 0, 0, errors.New("invalid argon2 threads")
	}
	if p64 == 0 || p64 > 255 {
		return 0, 0, 0, errors.New("invalid argon2 threads")
	}
	return uint32(mem64), uint32(t64), uint8(p64), nil
}
