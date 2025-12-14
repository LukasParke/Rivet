package security

import "testing"

func TestValidateOutboundURLRejectsPrivateWhenDisallowed(t *testing.T) {
	_, err := ValidateOutboundURL("http://127.0.0.1:8080", OutboundPolicy{AllowInternal: false})
	if err == nil {
		t.Fatalf("expected error")
	}
}

func TestValidateOutboundURLAllowsPrivateWhenAllowed(t *testing.T) {
	_, err := ValidateOutboundURL("http://127.0.0.1:8080", OutboundPolicy{AllowInternal: true})
	if err != nil {
		t.Fatalf("expected no error, got %v", err)
	}
}
