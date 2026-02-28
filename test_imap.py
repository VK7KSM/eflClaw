"""Quick IMAP login test for Gmail - bypasses ZeroClaw entirely."""
import imaplib

HOST = "imap.gmail.com"
PORT = 993
USER = "v601tv@gmail.com"
PASS = "iualdfybvwmqedwz"  # App Password without spaces

print(f"Connecting to {HOST}:{PORT}...")
try:
    mail = imaplib.IMAP4_SSL(HOST, PORT)
    print(f"Connected. Server greeting: {mail.welcome}")
    print(f"Logging in as {USER}...")
    mail.login(USER, PASS)
    print("✅ LOGIN SUCCESS!")
    mail.logout()
except imaplib.IMAP4.error as e:
    print(f"❌ LOGIN FAILED: {e}")
except Exception as e:
    print(f"❌ CONNECTION ERROR: {e}")
