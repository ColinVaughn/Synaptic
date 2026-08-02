import stripe

def create_customer(email):
    return stripe.Customer.create(email=email)
