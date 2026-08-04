import os
import signal
import time

running = True


def stop(_signal, _frame):
    global running
    running = False


signal.signal(signal.SIGTERM, stop)
while running:
    time.sleep(1)

print(f"worker {os.getpid()} encerrado")
