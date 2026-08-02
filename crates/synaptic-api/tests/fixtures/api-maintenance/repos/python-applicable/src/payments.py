import requests
import stripe

def create_customer_with_sdk(email):
    return stripe.Customer.create(email=email)

def create_customer_direct(email):
    return requests.post("https://api.stripe.com/v1/customers", json={"email": email})
