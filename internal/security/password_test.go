package security

import "testing"

func TestHashAndVerifyPassword(t *testing.T) {
	h, err := HashPassword("this is a long enough password")
	if err != nil {
		t.Fatalf("hash: %v", err)
	}
	ok, err := VerifyPassword("this is a long enough password", h.Encoded)
	if err != nil {
		t.Fatalf("verify: %v", err)
	}
	if !ok {
		t.Fatalf("expected ok")
	}

	ok, err = VerifyPassword("wrong password", h.Encoded)
	if err != nil {
		t.Fatalf("verify wrong: %v", err)
	}
	if ok {
		t.Fatalf("expected not ok")
	}
}
