import time

from tqdm import tqdm

for x in tqdm(range(10)):
    tqdm.write(f"Doing stuff {x + 1}")
    time.sleep(0.1)
