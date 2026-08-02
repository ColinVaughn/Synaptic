from .payments import create_customer_with_sdk

def onboard_customer(email):
    return create_customer_with_sdk(email)
