// Command iam-refresher exchanges VNG IAM client credentials for a bearer
// token and writes it into a Kubernetes Secret that an AgentgatewayModel
// references for the aiplatform vLLM backend.
//
// Tokens are short-lived (the live endpoint returns expires_in: 1800), so this
// runs as a CronJob on a cadence comfortably shorter than that.
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"os"
	"strings"
	"time"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/kubernetes"
	"k8s.io/client-go/rest"
)

// exchange trades IAM client credentials for a bearer token.
//
// The contract is HTTP Basic auth plus a form-encoded client_credentials
// grant, mirroring kong/llm/iam/accesstoken.lua:32-36. Sending the credentials
// as a JSON body instead returns 400 REQUEST_BODY_INVALID.
func exchange(authURL, accessKey, secretKey string) (string, error) {
	req, err := http.NewRequest(
		http.MethodPost,
		authURL,
		strings.NewReader("grant_type=client_credentials"),
	)
	if err != nil {
		return "", err
	}
	req.SetBasicAuth(accessKey, secretKey)
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")

	resp, err := (&http.Client{Timeout: 10 * time.Second}).Do(req)
	if err != nil {
		return "", fmt.Errorf("iam auth request failed: %w", err)
	}
	defer resp.Body.Close()

	// Deliberately does not echo the response body or the credentials: this
	// error reaches the pod log.
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("iam auth returned %d", resp.StatusCode)
	}

	var out struct {
		AccessToken string `json:"access_token"`
		ExpiresIn   int    `json:"expires_in"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&out); err != nil {
		return "", fmt.Errorf("decoding iam auth response: %w", err)
	}
	if out.AccessToken == "" {
		return "", fmt.Errorf("iam auth returned an empty access_token")
	}
	return out.AccessToken, nil
}

func mustEnv(name string) string {
	v := os.Getenv(name)
	if v == "" {
		log.Fatalf("%s is required", name)
	}
	return v
}

func main() {
	authURL := mustEnv("IAM_AUTH_URL")
	ns := mustEnv("TARGET_NAMESPACE")
	secretName := mustEnv("TARGET_SECRET")

	token, err := exchange(authURL, mustEnv("IAM_ACCESS_KEY"), mustEnv("IAM_SECRET_KEY"))
	if err != nil {
		log.Fatalf("token exchange failed: %v", err)
	}

	cfg, err := rest.InClusterConfig()
	if err != nil {
		log.Fatalf("in-cluster config: %v", err)
	}
	cs, err := kubernetes.NewForConfig(cfg)
	if err != nil {
		log.Fatalf("clientset: %v", err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	sec, err := cs.CoreV1().Secrets(ns).Get(ctx, secretName, metav1.GetOptions{})
	if err != nil {
		log.Fatalf("get secret: %v", err)
	}
	if sec.Data == nil {
		sec.Data = map[string][]byte{}
	}
	sec.Data["key"] = []byte(token)
	if _, err := cs.CoreV1().Secrets(ns).Update(ctx, sec, metav1.UpdateOptions{}); err != nil {
		log.Fatalf("update secret: %v", err)
	}
	log.Printf("refreshed %s/%s (token length %d)", ns, secretName, len(token))
}
