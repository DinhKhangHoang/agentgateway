package main

import (
	"encoding/base64"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

// The VNG IAM contract is NOT the JSON {clientId, clientSecret} body the pilot
// plan assumed — that form returns 400 REQUEST_BODY_INVALID against the live
// endpoint. The real contract, mirrored from Kong's
// kong/llm/iam/accesstoken.lua:32-36, is HTTP Basic auth carrying
// access_key:secret_key plus a form-encoded grant_type=client_credentials body.
// These tests pin that contract so a future edit cannot quietly regress it.

func TestExchangeUsesBasicAuthAndClientCredentialsGrant(t *testing.T) {
	var gotAuth, gotBody, gotContentType, gotMethod string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotMethod = r.Method
		gotAuth = r.Header.Get("Authorization")
		gotContentType = r.Header.Get("Content-Type")
		b, _ := io.ReadAll(r.Body)
		gotBody = string(b)
		json.NewEncoder(w).Encode(map[string]any{"access_token": "tok-123", "expires_in": 1800})
	}))
	defer srv.Close()

	if _, err := exchange(srv.URL, "ak", "sk"); err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	if gotMethod != http.MethodPost {
		t.Errorf("method = %q, want POST", gotMethod)
	}
	want := "Basic " + base64.StdEncoding.EncodeToString([]byte("ak:sk"))
	if gotAuth != want {
		t.Errorf("Authorization = %q, want %q", gotAuth, want)
	}
	if gotContentType != "application/x-www-form-urlencoded" {
		t.Errorf("Content-Type = %q, want application/x-www-form-urlencoded", gotContentType)
	}
	if gotBody != "grant_type=client_credentials" {
		t.Errorf("body = %q, want grant_type=client_credentials", gotBody)
	}
}

func TestExchangeReturnsAccessToken(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		json.NewEncoder(w).Encode(map[string]any{"access_token": "tok-123", "expires_in": 1800})
	}))
	defer srv.Close()

	got, err := exchange(srv.URL, "ak", "sk")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if got != "tok-123" {
		t.Fatalf("got %q, want %q", got, "tok-123")
	}
}

func TestExchangeErrorsOnNon200(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
	}))
	defer srv.Close()

	if _, err := exchange(srv.URL, "ak", "sk"); err == nil {
		t.Fatal("expected an error for 401, got nil")
	}
}

func TestExchangeErrorsOnEmptyToken(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		json.NewEncoder(w).Encode(map[string]any{"access_token": "", "expires_in": 1800})
	}))
	defer srv.Close()

	if _, err := exchange(srv.URL, "ak", "sk"); err == nil {
		t.Fatal("expected an error for an empty access_token, got nil")
	}
}

func TestExchangeErrorsOnMalformedJSON(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		io.WriteString(w, "not json")
	}))
	defer srv.Close()

	if _, err := exchange(srv.URL, "ak", "sk"); err == nil {
		t.Fatal("expected an error for a malformed body, got nil")
	}
}

// A failed exchange must never be mistaken for a token. Guard against the
// error message itself leaking the secret key into logs.
func TestExchangeErrorDoesNotLeakSecret(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusForbidden)
	}))
	defer srv.Close()

	_, err := exchange(srv.URL, "ak-visible", "sk-super-secret")
	if err == nil {
		t.Fatal("expected an error")
	}
	if strings.Contains(err.Error(), "sk-super-secret") {
		t.Fatalf("error leaks the secret key: %v", err)
	}
}
